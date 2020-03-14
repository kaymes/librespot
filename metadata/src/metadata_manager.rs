use cached::stores::SizedCache;
use cached::Cached;
use futures;

use crate::AudioItem;
use futures::sync::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::{Async, Future, Stream};
use librespot_core::session::Session;
use librespot_core::spotify_id::SpotifyId;

#[derive(Hash, PartialEq, Eq, Clone, Copy)]
pub enum MetadataKey {
    TrackInfo(SpotifyId),
}

#[derive(Clone)]
pub enum MetadataValue {
    TrackInfo { title: String, duration_ms: i32 },
}

#[derive(Clone)]
pub struct MetadataItem {
    pub key: MetadataKey,
    pub value: MetadataValue,
}

type UnsolicitedDataStream =
    Box<dyn futures::stream::Stream<Item = MetadataItem, Error = ()> + Send>;

enum MetadataManagerCommand {
    Query {
        key: MetadataKey,
        result_sender: futures::Complete<MetadataValue>,
    },
    SetSession(Session),
    SetUnsolicitedDataStream(UnsolicitedDataStream),
}

#[derive(Clone)]
pub struct MetadataManager {
    command_sender: UnboundedSender<MetadataManagerCommand>,
}

pub struct MetadataManagerInner {
    command_receiver: UnboundedReceiver<MetadataManagerCommand>,
    cache: SizedCache<MetadataKey, MetadataValue>,
    session: Session,
    unsolicited_data_stream: Option<UnsolicitedDataStream>,
    result_sender: UnboundedSender<MetadataItem>,
    result_receiver: UnboundedReceiver<MetadataItem>,
}

impl MetadataManager {
    pub fn new(session: Session) -> MetadataManager {
        let (command_sender, command_receiver) = unbounded();
        let (result_sender, result_receiver) = unbounded();

        let inner = MetadataManagerInner {
            command_receiver,
            cache: SizedCache::with_size(1000),
            session: session.clone(),
            unsolicited_data_stream: None,
            result_sender,
            result_receiver,
        };

        session.spawn(move |_| inner);

        MetadataManager { command_sender }
    }

    pub fn set_session(&mut self, session: Session) {
        self.command_sender
            .unbounded_send(MetadataManagerCommand::SetSession(session))
            .unwrap();
    }

    pub fn set_unsolicited_data_stream(&mut self, stream: UnsolicitedDataStream) {
        self.command_sender
            .unbounded_send(MetadataManagerCommand::SetUnsolicitedDataStream(stream))
            .unwrap();
    }

    pub fn query(&mut self, key: MetadataKey) -> futures::Oneshot<MetadataValue> {
        let (result_sender, result_receiver) = futures::oneshot();
        self.command_sender
            .unbounded_send(MetadataManagerCommand::Query { key, result_sender })
            .unwrap();
        return result_receiver;
    }
}

impl Future for MetadataManagerInner {
    type Item = ();
    type Error = ();
    fn poll(&mut self) -> futures::Poll<(), ()> {
        loop {
            // Store all results from our queries in the cache.
            if loop {
                match self.result_receiver.poll() {
                    Ok(Async::Ready(Some(item))) => self.cache.cache_set(item.key, item.value),
                    Ok(Async::Ready(None)) | Err(()) => break Err(()),
                    Ok(Async::NotReady) => break Ok(()),
                }
            }
            .is_err()
            {
                return Ok(Async::Ready(()));
            }

            // Store all unsolicited results in the cache.
            if loop {
                if let Some(unsolicited_data_stream) = &mut self.unsolicited_data_stream {
                    match unsolicited_data_stream.poll() {
                        Ok(Async::Ready(Some(item))) => self.cache.cache_set(item.key, item.value),
                        Ok(Async::Ready(None)) | Err(()) => break Err(()),
                        Ok(Async::NotReady) => break Ok(()),
                    }
                } else {
                    break Ok(());
                }
            }
            .is_err()
            {
                self.unsolicited_data_stream = None;
            }

            // process one command.
            match self.command_receiver.poll() {
                Ok(Async::Ready(Some(command))) => self.handle_command(command),
                Ok(Async::Ready(None)) => return Ok(Async::Ready(())),
                Ok(Async::NotReady) => return Ok(Async::NotReady),
                Err(()) => return Err(()),
            }

            // continue with the main loop
        }
    }
}

impl MetadataManagerInner {
    fn handle_command(&mut self, command: MetadataManagerCommand) {
        match command {
            MetadataManagerCommand::SetSession(session) => self.session = session,
            MetadataManagerCommand::SetUnsolicitedDataStream(stream) => {
                self.unsolicited_data_stream = Some(stream)
            }
            MetadataManagerCommand::Query { key, result_sender } => {
                if let Some(result) = self.cache.cache_get(&key) {
                    let _ = result_sender.send(result.clone());
                    return;
                } else {
                    // set up a future that prodces the MetadataValue for the query.
                    let request = match key {
                        MetadataKey::TrackInfo(id) => AudioItem::get_audio_item(&self.session, id)
                            .map(|audio_item| MetadataValue::TrackInfo {
                                title: audio_item.name,
                                duration_ms: audio_item.duration,
                            }),
                    };

                    // make the future put the result into the channels for the response and the cache.
                    let self_result_sender = self.result_sender.clone();
                    let request = request
                        .map(move |value| {
                            let _ = self_result_sender.unbounded_send(MetadataItem {
                                key,
                                value: value.clone(),
                            });
                            let _ = result_sender.send(value);
                        })
                        .map_err(|_| ());

                    // spawn the future so it gets run to completion by the session.
                    self.session.spawn(move |_| request);
                }
            }
        }
    }
}
