use std::collections::HashMap;
use std::sync::Arc;

use actix::prelude::*;
use anyhow;
use serde::Deserialize;
use serde_json;
use tokio::io::AsyncReadExt;

#[cfg(test)]
use serde::Serialize;

use crate::command_util;
use crate::config::ChannelConfig;
use crate::config::Config;
use crate::epg::*;
use crate::models::*;
use crate::tuner::*;

pub struct ClockSynchronizer<A: Actor> {
    config: Arc<Config>,
    tuner_manager: Addr<A>,
}

// TODO: The following implementation has code clones similar to
//       EitCollector and ServiceScanner.

impl<A> ClockSynchronizer<A>
where
    A: Actor,
    A: Handler<StartStreamingMessage>,
    A: Handler<StopStreamingMessage>,
    A::Context: actix::dev::ToEnvelope<A, StartStreamingMessage>,
    A::Context: actix::dev::ToEnvelope<A, StopStreamingMessage>,
{
    const LABEL: &'static str = "clock-synchronizer";

    pub fn new(
        config: Arc<Config>,
        tuner_manager: Addr<A>,
    ) -> Self {
        ClockSynchronizer { config, tuner_manager }
    }

    pub async fn sync_clocks(
        self
    ) -> Vec<(EpgChannel, Option<HashMap<ServiceTriple, Clock>>)> {
        tracing::debug!("Synchronizing clocks...");

        let command = &self.config.jobs.sync_clocks.command;
        let mut results = Vec::new();

        for channel in self.config.channels.iter() {
            let result = match Self::sync_clocks_in_channel(
                channel, command, &self.tuner_manager).await {
                Ok(clocks) => {
                    let mut map = HashMap::new();
                    for clock in clocks.into_iter() {
                        let triple = (clock.nid, clock.tsid, clock.sid).into();
                        map.insert(triple, clock.clock.clone());
                    }
                    Some(map)
                }
                Err(err) => {
                    tracing::warn!("Failed to synchronize clocks in {}: {}",
                                   channel.name, err);
                    None
                }
            };
            results.push((channel.clone().into(), result));
        }

        tracing::debug!("Synchronized {} channels", self.config.channels.len());

        results
    }

    async fn sync_clocks_in_channel(
        channel: &ChannelConfig,
        command: &str,
        tuner_manager: &Addr<A>,
    ) -> anyhow::Result<Vec<SyncClock>> {
        tracing::debug!("Synchronizing clocks in {}...", channel.name);

        let user = TunerUser {
            info: TunerUserInfo::Job { name: Self::LABEL.to_string() },
            priority: (-1).into(),
        };

        let stream = tuner_manager.send(StartStreamingMessage {
            channel: channel.clone().into(),
            user
        }).await??;

        let stop_trigger = TunerStreamStopTrigger::new(
            stream.id(), tuner_manager.clone().recipient());

        let template = mustache::compile_str(command)?;
        let data = mustache::MapBuilder::new()
            .insert("sids", &channel.services)?
            .insert("xsids", &channel.excluded_services)?
            .build();
        let cmd = template.render_data_to_string(&data)?;

        let mut pipeline = command_util::spawn_pipeline(
            vec![cmd], stream.id())?;

        let (input, mut output) = pipeline.take_endpoints().unwrap();

        let handle = tokio::spawn(stream.pipe(input));

        let mut buf = Vec::new();
        output.read_to_end(&mut buf).await?;

        drop(stop_trigger);

        // Explicitly dropping the output of the pipeline is needed.  The output
        // holds the child processes and it kills them when dropped.
        drop(pipeline);

        // Wait for the task so that the tuner is released before a request for
        // streaming in the next iteration.
        let _ = handle.await;

        anyhow::ensure!(!buf.is_empty(), "No clock, maybe out of service");

        let clocks: Vec<SyncClock> = serde_json::from_slice(&buf)?;
        tracing::debug!(
            "Synchronized {} clocks in {}", clocks.len(), channel.name);

        Ok(clocks)
    }
}

#[derive(Clone, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
#[serde(rename_all = "camelCase")]
struct SyncClock {
    nid: NetworkId,
    tsid: TransportStreamId,
    sid: ServiceId,
    clock: Clock,
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use crate::broadcaster::BroadcasterStream;
    use crate::error::Error;
    use crate::mpeg_ts_stream::MpegTsStream;

    type Mock = actix::actors::mocker::Mocker<TunerManager>;

    #[actix::test]
    async fn test_sync_clocks_in_channel() {
        let mock = Mock::mock(Box::new(|msg, _ctx| {
            if let Some(_) = msg.downcast_ref::<StartStreamingMessage>() {
                let (_, stream) = BroadcasterStream::new_for_test();
                let result: Result<_, Error> =
                    Ok(MpegTsStream::new(TunerSubscriptionId::default(), stream));
                Box::new(Some(result))
            } else if let Some(_) = msg.downcast_ref::<StopStreamingMessage>() {
                Box::new(Some(()))
            } else {
                unimplemented!();
            }
        })).start();

        let expected = vec![SyncClock {
            nid: 1.into(),
            tsid: 2.into(),
            sid: 3.into(),
            clock: Clock { pid: 1, pcr: 2, time: 3 },
        }];

        let config_yml = format!(r#"
            channels:
              - name: channel
                type: GR
                channel: '0'
            jobs:
              sync-clocks:
                command: echo '{}'
        "#, serde_json::to_string(&expected).unwrap());

        let config = Arc::new(
            serde_yaml::from_str::<Config>(&config_yml).unwrap());

        let sync = ClockSynchronizer::new(config, mock.clone());
        let results = sync.sync_clocks().await;
        assert_eq!(results.len(), 1);
        assert_matches!(&results[0], (_, Some(v)) => {
            let triple = (1, 2, 3).into();
            assert_eq!(v.len(), 1);
            assert!(v.contains_key(&triple));
            assert_matches!(v[&triple], Clock { pid, pcr, time } => {
                assert_eq!(pid, 1);
                assert_eq!(pcr, 2);
                assert_eq!(time, 3);
            });
        });

        // Emulate out of services by using `false`
        let config = Arc::new(serde_yaml::from_str::<Config>(r#"
            channels:
              - name: channel
                type: GR
                channel: '0'
            jobs:
              scan-services:
                command: false
        "#).unwrap());
        let sync = ClockSynchronizer::new(config, mock.clone());
        let results = sync.sync_clocks().await;
        assert_eq!(results.len(), 1);
        assert_matches!(&results[0], (_, None));
    }
}
