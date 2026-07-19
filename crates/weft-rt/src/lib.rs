//! # weft-rt — WEFT-RT voice SFU (§16)
//!
//! The reference [`VoiceBackend`](weft_core::VoiceBackend) implementation: an
//! embedded **Selective Forwarding Unit** built on `webrtc` (the
//! `RTCPeerConnection` API). It owns the UDP sockets + DTLS/ICE and forwards
//! each participant's Opus RTP to the room's other subscribers — the media
//! plane weft-core deliberately never touches.

#![forbid(unsafe_code)]

mod sfu;

pub use sfu::{SfuConfig, WebrtcSfu};
