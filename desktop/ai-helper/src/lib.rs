//! PhoneCast's local AI helper. Everything here runs on the CPU, inside a
//! separate process, and nothing leaves the PC except the connections to
//! YouTube/Twitch chat that the streamer switched on.
//!
//! The app talks to the helper over a loopback TCP socket with one JSON
//! object per line ([`proto`]). The helper never acts on its own: clips are
//! requested from the app, and chat replies are only sent when the streamer
//! enabled that reply path in settings.

pub mod faq;
pub mod models;
pub mod proto;
pub mod score;
pub mod spam;
pub mod sysusage;
pub mod twitch;
pub mod youtube;

#[cfg(feature = "llm")]
pub mod llm;
#[cfg(feature = "yamnet")]
pub mod yamnet;

pub mod secret;
