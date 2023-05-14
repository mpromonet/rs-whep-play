use anyhow::Result;
use serde_json::json;
use webrtc::util::Marshal;
use std::{env, sync::Arc};
use tokio::sync::Notify;

use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType};
use webrtc::track::track_remote::TrackRemote;
use gstreamer::prelude::*;

async fn handle_data(appsrc: &gstreamer_app::AppSrc, track: Arc<TrackRemote>, notify: Arc<Notify>) -> Result<()> {
    loop {
        tokio::select! {
            result = track.read_rtp() => {
                if let Ok((rtp_packet, _)) = result {
                    let buf = rtp_packet.marshal()?;
                    let buffer = gstreamer::Buffer::from_slice(buf);
                    let _ = appsrc.push_buffer(buffer);
                }else{
                    println!("read_rtp error");
                    return Ok(());
                }
            }
            _ = notify.notified() => {
                println!("notified");
                return Ok(());
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut url =
        "http://localhost:8000/api/whep?url=Zeeland&options=rtptransport%3dtcp%26timeout%3d60";
    let args: Vec<String> = env::args().collect();
    if args.len() > 1 {
        url = &args[1];
    }

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
            payload_type: 102,
            ..Default::default()
        },
        RTPCodecType::Video,
    )?;

    // Create the API object with the MediaEngine
    let api = APIBuilder::new()
        .with_media_engine(m)
        .build();

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
    peer_connection.add_transceiver_from_kind(RTPCodecType::Video, None).await?;

    let notify_tx = Arc::new(Notify::new());
    let notify_rx = notify_tx.clone();

    // Set a handler for when a new remote track starts
    peer_connection.on_track(Box::new(move |track, _, _| {
        let notify_rx2 = Arc::clone(&notify_rx);

        Box::pin(async move {
            let codec = track.codec();
            let mime_type = codec.capability.mime_type.to_lowercase();
            if mime_type == MIME_TYPE_H264.to_lowercase() {
                println!("Got h264 track, receiving data");

                let pipeline = gstreamer::Pipeline::new(None);
                let src = gstreamer::ElementFactory::make("appsrc").build().unwrap();
                let rtp = gstreamer::ElementFactory::make("rtph264depay").build().unwrap();
                let decode = gstreamer::ElementFactory::make("avdec_h264").build().unwrap();
                let videoconvert = gstreamer::ElementFactory::make("videoconvert").build().unwrap();
                let sink = gstreamer::ElementFactory::make("autovideosink").build().unwrap();
            
                pipeline.add_many(&[&src, &rtp, &decode, &videoconvert, &sink]).unwrap();
                gstreamer::Element::link_many(&[&src, &rtp, &decode, &videoconvert, &sink]).unwrap();            
                let appsrc = src.dynamic_cast::<gstreamer_app::AppSrc>().unwrap();
                appsrc.set_caps(Some(
                    &gstreamer::Caps::builder("application/x-rtp")
                        .field("media", "video")
                        .field("encoding-name", "H264")
                        .field("payload", 102i32)
                        .field("clock-rate", 90000i32)
                        .build()));
                appsrc.set_format(gstreamer::Format::Time);

                println!("appsrc {:?}", appsrc);
                let _ = pipeline.set_state(gstreamer::State::Playing);

                tokio::spawn(async move {
                    let _ = handle_data(&appsrc, track, notify_rx2).await;
                });
            }
        })
    }));

    let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<()>(1);

    // Set the handler for ICE connection state
    // This will notify you when the peer has connected/disconnected
    peer_connection.on_ice_connection_state_change(Box::new(
        move |connection_state: RTCIceConnectionState| {
            println!("Connection State has changed {connection_state}");

            if connection_state == RTCIceConnectionState::Failed {
                notify_tx.notify_waiters();
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
    println!("Offer:{offer_str}");
    let client = reqwest::Client::new();
    let response = client.post(url).body(offer_str).send().await?;
    let answer_str = response.text().await?;
    println!("Answer:{answer_str}");
    let desc = json!({ "type": "answer", "sdp": answer_str }).to_string();
    let answer = serde_json::from_str::<RTCSessionDescription>(&desc)?;

    // Set remote SessionDescription
    peer_connection.set_remote_description(answer).await?;

    println!("Press ctrl-c to stop");
    tokio::select! {
        _ = done_rx.recv() => {
            println!("received done signal!");
        }
        _ = tokio::signal::ctrl_c() => {
            println!();
        }
    };

    peer_connection.close().await?;

    Ok(())
}
