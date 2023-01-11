use std::convert::Infallible;

use super::*;

use axum::response::sse::Event;
use axum::response::sse::Sse;
use futures::stream::Stream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::models::events::*;

pub(super) async fn events<E, R, O>(
    State(EpgExtractor(epg)): State<EpgExtractor<E>>,
    State(RecordingManagerExtractor(recording_manager)): State<RecordingManagerExtractor<R>>,
    State(OnairProgramManagerExtractor(onair_manager)): State<OnairProgramManagerExtractor<O>>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Error>
where
    E: Call<crate::epg::RegisterEmitter>,
    R: Call<crate::recording::RegisterEmitter>,
    O: Call<crate::onair::RegisterEmitter>,
{
    let (sender, receiver) = mpsc::channel(32);

    let feeder = EventFeeder(sender);

    epg.call(crate::epg::RegisterEmitter::ProgramsUpdated(
        feeder.clone().into(),
    ))
    .await?;

    recording_manager
        .call(crate::recording::RegisterEmitter::RecordingStarted(
            feeder.clone().into(),
        ))
        .await?;

    recording_manager
        .call(crate::recording::RegisterEmitter::RecordingStopped(
            feeder.clone().into(),
        ))
        .await?;

    recording_manager
        .call(crate::recording::RegisterEmitter::RecordingFailed(
            feeder.clone().into(),
        ))
        .await?;

    recording_manager
        .call(crate::recording::RegisterEmitter::RecordingRescheduled(
            feeder.clone().into(),
        ))
        .await?;

    onair_manager
        .call(crate::onair::RegisterEmitter(feeder.clone().into()))
        .await?;

    Ok(Sse::new(ReceiverStream::new(receiver)).keep_alive(Default::default()))
}

#[derive(Clone)]
struct EventFeeder(mpsc::Sender<Result<Event, Infallible>>);

macro_rules! impl_emit {
    ($msg:path, $event:ty) => {
        #[async_trait]
        impl Emit<$msg> for EventFeeder {
            async fn emit(&self, msg: $msg) {
                let event = Event::default()
                    .event(<$event>::name())
                    .json_data(<$event>::from(msg))
                    .unwrap();
                if let Err(_) = self.0.send(Ok(event)).await {
                    tracing::warn!("Client disconnected");
                }
            }
        }

        impl Into<Emitter<$msg>> for EventFeeder {
            fn into(self) -> Emitter<$msg> {
                Emitter::new(self)
            }
        }
    };
}

impl_emit!(crate::epg::ProgramsUpdated, EpgProgramsUpdated);
impl_emit!(crate::recording::RecordingStarted, RecordingStarted);
impl_emit!(crate::recording::RecordingStopped, RecordingStopped);
impl_emit!(crate::recording::RecordingFailed, RecordingFailed);
impl_emit!(crate::recording::RecordingRescheduled, RecordingRescheduled);
impl_emit!(crate::onair::OnairProgramChanged, OnairProgramChanged);
