//! The toolkit-free half of the HypoDJ desktop apps. `view` turns MPD pairs into
//! rows and words (pure, tested without a display); `worker` owns the sockets on std
//! threads and answers with [`worker::Msg`] values over an `async_channel`. The GTK
//! app (djg) and the Qt app (djq) both fold those messages into their own widgets,
//! so the two shells show the same words for the same daemon state by construction.

pub mod view;
pub mod worker;
