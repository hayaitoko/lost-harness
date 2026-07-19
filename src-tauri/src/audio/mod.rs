// M6: Audio engine, VAD, TTS pipeline.
//
// The native STT/TTS/AEC backends (whisper, piper, Voice-Processing-IO) are the
// on-hardware M6 build (behind a future `voice` cargo feature). What lands here
// now is the NON-native security core: the audio-egress gate — the new privacy
// invariant voice introduces — which is pure policy over the existing §7 gate
// and fully unit-tested.

pub mod privacy;
