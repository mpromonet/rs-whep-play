/* ---------------------------------------------------------------------------
** This software is in the public domain, furnished "as is", without technical
** support, and with no warranty, express or implied, as to its usefulness for
** any purpose.
**
** main.rs
**
** -------------------------------------------------------------------------*/

use anyhow::Result;
use serde_json::json;
use std::{env, sync::Arc};
use webrtc::util::Marshal;

use gstreamer::prelude::*;
use log::{info, trace};
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::track::track_remote::TrackRemote;

async fn handle_data(appsrc: &gstreamer_app::AppSrc, track: Arc<TrackRemote>) -> Result<()> {
    loop {
        tokio::select! {
            result = track.read_rtp() => {
                if let Ok((rtp_packet, _)) = result {
                    trace!("rtp:{rtp_packet}");
                    let buf = rtp_packet.marshal()?;
                    let buffer = gstreamer::Buffer::from_slice(buf);
                    let _ = appsrc.push_buffer(buffer);
                }else{
                    info!("read_rtp error");
                    return Ok(());
                }
            }
        }
    }
}

async fn whep(url: &str, offer_str: String) -> Result<String> {
    info!("Offer:{offer_str}");
    let client = reqwest::Client::new();
    let response = client.post(url).body(offer_str).send().await?;
    let answer_str = response.text().await?;
    info!("Answer:{answer_str}");
    Ok(answer_str)
}

fn create_h264_consumer(payload_type: u8) -> Result<gstreamer_app::AppSrc> {
    let pipeline = gstreamer::Pipeline::new(None);
    let src = gstreamer::ElementFactory::make("appsrc").build()?;
    let rtp = gstreamer::ElementFactory::make("rtph264depay").build()?;
    let decode = gstreamer::ElementFactory::make("avdec_h264").build()?;
    let videoconvert = gstreamer::ElementFactory::make("videoconvert").build()?;
    let sink = gstreamer::ElementFactory::make("autovideosink").build()?;

    pipeline
        .add_many(&[&src, &rtp, &decode, &videoconvert, &sink])?;
    gstreamer::Element::link_many(&[&src, &rtp, &decode, &videoconvert, &sink])?;
    let appsrc = src.dynamic_cast::<gstreamer_app::AppSrc>().unwrap();
    appsrc.set_caps(Some(
        &gstreamer::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("encoding-name", "H264")
            .field("payload", payload_type)
            .field("clock-rate", 90000)
            .build(),
    ));
    appsrc.set_format(gstreamer::Format::Time);

    info!("appsrc {:?}", appsrc);

    // start pipeline
    let _ = pipeline.set_state(gstreamer::State::Playing);

    Ok(appsrc)
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut url =
        "http://localhost:8000/api/whep?url=Zeeland&options=rtptransport%3dtcp%26timeout%3d60";
    let mut payload_type = 102u8;
    let args: Vec<String> = env::args().collect();
    if args.len() > 1 {
        url = &args[1];
    }
    if args.len() > 2 {
        payload_type = args[2].parse::<u8>().unwrap();
    }

    // init logger
    env_logger::init();

    // gstreamer pipeline
    gstreamer::init()?;

    // Create a MediaEngine object to configure the supported codec
    let mut m = MediaEngine::default();

    // Setup the codecs you want to use.
    m.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line: "".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type,
            ..Default::default()
        },
        RTPCodecType::Video,
    )?;

    // Create the API object with the MediaEngine
    let api = APIBuilder::new().with_media_engine(m).build();

    // Prepare the configuration
    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            ..Default::default()
        }],
        ..Default::default()
    };

    // Create a new RTCPeerConnection
    let peer_connection = Arc::new(api.new_peer_connection(config).await?);

    // Allow us to receive 1 video track
    peer_connection
        .add_transceiver_from_kind(RTPCodecType::Video, None)
        .await?;

    // Set a handler for when a new remote track starts
    peer_connection.on_track(Box::new(move |track, _, _| {
        Box::pin(async move {
            let codec = track.codec();
            let mime_type = codec.capability.mime_type.to_lowercase();
            if mime_type == MIME_TYPE_H264.to_lowercase() {
                info!("Got h264 track, receiving data");

                let appsrc = create_h264_consumer(payload_type).unwrap();

                tokio::spawn(async move {
                    let _ = handle_data(&appsrc, track).await;
                });
            }
        })
    }));

    let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<()>(1);

    // Set the handler for ICE connection state
    // This will notify you when the peer has connected/disconnected
    peer_connection.on_ice_connection_state_change(Box::new(
        move |connection_state: RTCIceConnectionState| {
            info!("Connection State has changed {connection_state}");

            if connection_state == RTCIceConnectionState::Failed {
                let _ = done_tx.try_send(());
            }
            Box::pin(async {})
        },
    ));

    // Create offer
    let offer = peer_connection.create_offer(None).await?;
    let offer_str = serde_json::to_string(&offer.sdp)?;

    // Set local SessionDescription
    peer_connection.set_local_description(offer).await?;

    // Wait ICE Gathering is complete
    let mut gather_complete = peer_connection.gathering_complete_promise().await;
    let _ = gather_complete.recv().await;

    // WHEP call
    let answer_str = whep(url, offer_str).await?;
    let desc = json!({ "type": "answer", "sdp": answer_str }).to_string();
    let answer = serde_json::from_str::<RTCSessionDescription>(&desc)?;

    // Set remote SessionDescription
    peer_connection.set_remote_description(answer).await?;

    tokio::select! {
        _ = done_rx.recv() => {
            info!("received done signal!");
        }
        _ = tokio::signal::ctrl_c() => {
            println!();
        }
    };

    peer_connection.close().await?;

    Ok(())
}
