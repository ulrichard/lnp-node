// LNP Node: node running lightning network protocol and generalized lightning
// channels.
// Written in 2020 by
//     Dr. Maxim Orlovsky <orlovsky@pandoracore.com>
//
// To the extent possible under law, the author(s) have dedicated all
// copyright and related and neighboring rights to this software to
// the public domain worldwide. This software is distributed without
// any warranty.
//
// You should have received a copy of the MIT License
// along with this software.
// If not, see <https://opensource.org/licenses/MIT>.

use bitcoin::secp256k1::Signature;
use lnp::bolt::Lifecycle;
use lnp::p2p::legacy::{ActiveChannelId, ChannelId, FundingCreated, Messages as LnMsg};
use lnp::Extension;
use microservices::esb::Handler;
use wallet::address::AddressCompat;

use super::Error;
use crate::automata::{Event, StateMachine};
use crate::bus::{BusMsg, CtlMsg, FundChannel, OpenChannelWith};
use crate::channeld::automata;
use crate::channeld::runtime::Runtime;
use crate::rpc::ServiceId;
use crate::service::LogStyle;
use crate::{CtlServer, Endpoints};

/// Channel proposal workflow
#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Hash, Debug, Display)]
#[derive(StrictEncode, StrictDecode)]
pub enum ChannelPropose {
    /// asked remote peer to accept a new channel
    #[display("PROPOSED")]
    Proposed,

    /// remote peer accepted our channel proposal
    #[display("ACCEPTED")]
    Accepted,

    /// signing refund transaction on our side
    #[display("SIGNING")]
    Signing,

    /// sent funding txid and commitment signature to the remote peer
    #[display("FUNDING")]
    Funding,

    /// received signed commitment from the remote peer
    #[display("SIGNED")]
    Signed,

    /// awaiting funding transaction to be mined
    #[display("FUNDED")]
    Funded,

    /// funding transaction is mined, awaiting for the other peer confirmation of this fact
    #[display("LOCKED")]
    Locked,
}

impl StateMachine<BusMsg, Runtime> for ChannelPropose {
    type Error = automata::Error;

    fn next(
        self,
        event: Event<BusMsg>,
        runtime: &mut Runtime,
    ) -> Result<Option<Self>, Self::Error> {
        let channel_id = runtime.channel.active_channel_id();
        debug!("ChannelPropose {:#} received {} event", channel_id, event.message);
        let state = match self {
            ChannelPropose::Proposed => complete_proposed(event, runtime),
            ChannelPropose::Accepted => complete_accepted(event, runtime),
            ChannelPropose::Signing => complete_signing(event, runtime),
            ChannelPropose::Funding => complete_funding(event, runtime),
            ChannelPropose::Signed => complete_signed(event, runtime),
            ChannelPropose::Funded => complete_funded(event, runtime),
            ChannelPropose::Locked => {
                complete_locked(event, runtime)?;
                info!("ChannelPropose {:#} has completed its work", channel_id);
                return Ok(None);
            }
        }?;
        info!("ChannelPropose {:#} switched to {} state", channel_id, state);
        Ok(Some(state))
    }
}

impl ChannelPropose {
    /// Computes channel lifecycle stage for the current channel proposal workflow stage
    pub fn lifecycle(&self) -> Lifecycle {
        match self {
            ChannelPropose::Proposed => Lifecycle::Proposed,
            ChannelPropose::Accepted => Lifecycle::Accepted,
            ChannelPropose::Signing => Lifecycle::Signing,
            ChannelPropose::Funding => Lifecycle::Funding,
            ChannelPropose::Signed => Lifecycle::Signed,
            ChannelPropose::Funded => Lifecycle::Funded,
            ChannelPropose::Locked => Lifecycle::Locked,
        }
    }
}

// State transitions:

impl ChannelPropose {
    /// Constructs channel proposal state machine
    pub fn with(
        runtime: &mut Runtime,
        endpoints: &mut Endpoints,
        request: OpenChannelWith,
    ) -> Result<ChannelPropose, automata::Error> {
        let open_channel = LnMsg::OpenChannel(runtime.channel.compose_open_channel(
            request.funding_sat,
            request.push_msat,
            request.policy,
            request.common_params,
            request.local_params,
            request.local_keys,
        )?);

        runtime.send_p2p(endpoints, open_channel)?;

        Ok(ChannelPropose::Proposed)
    }

    /// Construct information message for error and client reporting
    pub fn info_message(&self, channel_id: ActiveChannelId) -> String {
        match self {
            ChannelPropose::Proposed => format!(
                "{} to remote peer (using temp id {:#})",
                "Proposing channel".promo(),
                channel_id.promoter()
            ),
            ChannelPropose::Accepted => format!(
                "Remote peer {} channel with temp id {:#}. Constructing refund transaction.",
                "accepted".promo(),
                channel_id.promoter()
            ),
            ChannelPropose::Signing => format!(
                "{} refund transaction locally for channel {:#}",
                "Signing".promoter(),
                channel_id.promoter()
            ),
            ChannelPropose::Funding => format!(
                "{} for the remote peer to sign refund transaction for channel {:#}",
                "Awaiting".promo(),
                channel_id.promoter()
            ),
            ChannelPropose::Signed => format!(
                "{} funding transaction for channel {:#}",
                "Signing".promo(),
                channel_id.promoter()
            ),
            ChannelPropose::Funded => format!(
                "{} fully signed funding transaction for channel {:#}",
                "Publishing".promo(),
                channel_id.promoter()
            ),
            ChannelPropose::Locked => {
                format!("{} channel {:#}", "Activating".promo(), channel_id.promoter())
            }
        }
    }
}

fn complete_proposed(
    event: Event<BusMsg>,
    runtime: &mut Runtime,
) -> Result<ChannelPropose, automata::Error> {
    let accept_channel = match event.message {
        BusMsg::Ln(LnMsg::AcceptChannel(accept_channel)) => accept_channel,
        wrong_msg => {
            return Err(Error::UnexpectedMessage(wrong_msg, Lifecycle::Proposed, event.source))
        }
    };

    let channel = &mut runtime.channel;
    channel.update_from_peer(&LnMsg::AcceptChannel(accept_channel))?;

    let fund_channel = FundChannel {
        script_pubkey: channel.funding_script_pubkey(),
        feerate_per_kw: None, // Will use one from the funding wallet
        amount: channel.funding().amount(),
    };

    if let Some(address) = channel
        .network()
        .and_then(|network| AddressCompat::from_script(&fund_channel.script_pubkey, network))
    {
        debug!("Channel funding address is {}", address);
    }

    runtime.send_ctl(
        event.endpoints,
        ServiceId::LnpBroker,
        CtlMsg::ConstructFunding(fund_channel),
    )?;
    Ok(ChannelPropose::Accepted)
}

fn complete_accepted(
    event: Event<BusMsg>,
    runtime: &mut Runtime,
) -> Result<ChannelPropose, automata::Error> {
    let funding_psbt = match event.message {
        BusMsg::Ctl(CtlMsg::FundingConstructed(funding_psbt)) => funding_psbt,
        wrong_msg => {
            return Err(Error::UnexpectedMessage(wrong_msg, Lifecycle::Accepted, event.source))
        }
    };

    trace!("Funding transaction: {:#?}", funding_psbt);
    debug!("Funding transaction id is {}", funding_psbt.global.unsigned_tx.txid());

    let channel = &mut runtime.channel;
    let refund_psbt = channel.refund_tx(funding_psbt, true)?;

    trace!("Refund transaction: {:#?}", refund_psbt);
    trace!("Local keyset: {:#}", channel.constructor().local_keys());
    trace!("Remote keyset: {:#}", channel.constructor().remote_keys());
    debug!("Refund transaction id is {}", refund_psbt.global.unsigned_tx.txid());

    runtime.send_ctl(event.endpoints, ServiceId::Signer, CtlMsg::Sign(refund_psbt))?;
    Ok(ChannelPropose::Signing)
}

fn complete_signing(
    mut event: Event<BusMsg>,
    runtime: &mut Runtime,
) -> Result<ChannelPropose, automata::Error> {
    let refund_psbt = match event.message {
        BusMsg::Ctl(CtlMsg::Signed(psbt)) => psbt,
        wrong_msg => {
            return Err(Error::UnexpectedMessage(wrong_msg, Lifecycle::Signing, event.source))
        }
    };

    let channel = &runtime.channel;

    let funding_pubkey = channel.funding_pubkey();
    let funding_input =
        refund_psbt.inputs.get(0).expect("BOLT commitment always has a single input");
    let signature = funding_input
        .partial_sigs
        .get(&bitcoin::PublicKey::new(funding_pubkey))
        .ok_or(automata::Error::FundingPsbtUnsigned(funding_pubkey))?;
    // TODO: Use BitcoinSignature type for parsing signature once bitcoin 0.27 is released
    let signature = Signature::from_der(&signature[..signature.len() - 1])
        .map_err(automata::Error::InvalidSig)?;

    let funding = channel.funding();
    let (funding_txid, funding_output_index) = (funding.txid(), funding.output());
    let funding_created = FundingCreated {
        temporary_channel_id: channel
            .temp_channel_id()
            .expect("channel at funding stage must have temporary channel id"),
        funding_txid,
        funding_output_index,
        signature,
    };

    let new_id = ServiceId::Channel(ChannelId::with(funding_txid, funding_output_index));
    debug!("Changing channeld identifier from {} to {}", runtime.identity(), new_id);
    runtime.set_identity(&mut event.endpoints, new_id).expect("unrecoverable ZMQ failure");
    // needed to update ESB routing map
    runtime.send_ctl(event.endpoints, ServiceId::LnpBroker, CtlMsg::Hello)?;

    runtime.send_p2p(event.endpoints, LnMsg::FundingCreated(funding_created))?;
    Ok(ChannelPropose::Funding)
}

fn complete_funding(
    event: Event<BusMsg>,
    runtime: &mut Runtime,
) -> Result<ChannelPropose, automata::Error> {
    let funding_signed = match event.message {
        BusMsg::Ln(LnMsg::FundingSigned(funding_signed)) => funding_signed,
        wrong_msg => {
            return Err(Error::UnexpectedMessage(wrong_msg, Lifecycle::Funding, event.source))
        }
    };

    debug!("Got remote node signature {}", funding_signed.signature);

    // Save signature
    runtime.channel.update_from_peer(&LnMsg::FundingSigned(funding_signed))?;

    runtime.send_ctl(event.endpoints, ServiceId::LnpBroker, CtlMsg::PublishFunding)?;
    Ok(ChannelPropose::Signed)
}

fn complete_signed(
    event: Event<BusMsg>,
    runtime: &mut Runtime,
) -> Result<ChannelPropose, automata::Error> {
    if !matches!(event.message, BusMsg::Ctl(CtlMsg::FundingPublished)) {
        return Err(Error::UnexpectedMessage(event.message, Lifecycle::Signed, event.source));
    }

    let channel = &runtime.channel;
    let txid = channel.funding().txid();
    debug!("Funding transaction {} is published", txid);

    runtime.send_ctl(event.endpoints, ServiceId::Chain, CtlMsg::Track(txid))?;
    Ok(ChannelPropose::Funded)
}

fn complete_funded(
    _event: Event<BusMsg>,
    _runtime: &mut Runtime,
) -> Result<ChannelPropose, automata::Error> {
    todo!()
}

fn complete_locked(_event: Event<BusMsg>, _runtime: &mut Runtime) -> Result<(), automata::Error> {
    todo!()
}