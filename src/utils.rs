/* ---------------------------------------------------------------------------
** This software is in the public domain, furnished "as is", without technical
** support, and with no warranty, express or implied, as to its usefulness for
** any purpose.
**
** utils.rs
**
** -------------------------------------------------------------------------*/

use anyhow::Result;

use std::sync::Arc;
use webrtc::util::Marshal;

use gstreamer::prelude::*;
use log::*;

use webrtc::track::track_remote::TrackRemote;

pub async fn whep(url: &str, offer_sdp: String) -> Result<String> {
    info!("Offer:{offer_sdp}");
    let client = reqwest::Client::new();
    let response = client.post(url).body(offer_sdp).send().await?;
    let answer_sdp = response.text().await?;
    info!("Answer:{answer_sdp}");
    Ok(answer_sdp)
}

pub fn create_processing(payload_type: u8, clock_rate: u32, codec: &str, track: Arc<TrackRemote>) {
    let (pipeline, appsrc) = create_pipeline(payload_type, clock_rate, codec).unwrap();
    let _ = pipeline.set_state(gstreamer::State::Playing);

    tokio::spawn(async move {
        let _ = handle_data(&appsrc, track).await;
    });
}

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

fn create_pipeline(
    payload_type: u8,
    clock_rate: u32,
    mimetype: &str,
) -> Result<(gstreamer::Pipeline, gstreamer_app::AppSrc)> {
    let rtpdepay;
    let decoder;
    let codec;

    match mimetype {
        "video/H265" => {
            rtpdepay = "rtph265depay";
            decoder = "avdec_h265";
            codec = "H265";
        }
        "video/H264" => {
            rtpdepay = "rtph264depay";
            decoder = "avdec_h264";
            codec = "H264";
        }
        "video/VP8" => {
            rtpdepay = "rtpvp8depay";
            decoder = "avdec_vp8";
            codec = "VP8";
        }
        "video/VP9" => {
            rtpdepay = "rtpvp9depay";
            decoder = "avdec_vp9";
            codec = "VP9";
        }
        _ => {
            unimplemented!("mimetype:{mimetype} not managed");
        }
    }

    let pipeline = gstreamer::Pipeline::new();
    let src = gstreamer::ElementFactory::make("appsrc").build()?;
    let rtp = gstreamer::ElementFactory::make(rtpdepay).build()?;
    let decode = gstreamer::ElementFactory::make(decoder).build()?;
    let videoconvert = gstreamer::ElementFactory::make("videoconvert").build()?;
    let sink = gstreamer::ElementFactory::make("autovideosink").build()?;

    pipeline.add_many(&[&src, &rtp, &decode, &videoconvert, &sink])?;
    gstreamer::Element::link_many(&[&src, &rtp, &decode, &videoconvert, &sink])?;

    let appsrc = configure_appsrc(src, payload_type, clock_rate, codec)?;

    Ok((pipeline, appsrc))
}

fn configure_appsrc(
    src: gstreamer::Element,
    payload_type: u8,
    clock_rate: u32,
    codec: &str,
) -> Result<gstreamer_app::AppSrc> {
    let appsrc = src.dynamic_cast::<gstreamer_app::AppSrc>().unwrap();

    appsrc.set_caps(Some(
        &gstreamer::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("encoding-name", codec)
            .field("payload", payload_type)
            .field("clock-rate", clock_rate as i32)
            .build(),
    ));
    appsrc.set_format(gstreamer::Format::Time);

    info!("appsrc {:?}", appsrc);

    Ok(appsrc)
}
