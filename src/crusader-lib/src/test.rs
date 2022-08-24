use bytes::{Bytes, BytesMut};
use futures::future::FutureExt;
use futures::{pin_mut, select, Sink, Stream};
use futures::{stream, StreamExt};
use rand::prelude::StdRng;
use rand::Rng;
use rand::SeedableRng;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::{
    error::Error,
    io::Cursor,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::io::AsyncWriteExt;
use tokio::join;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{oneshot, watch, Semaphore};
use tokio::task::{self, yield_now, JoinHandle};
use tokio::{
    net::{self},
    time,
};
use tokio_util::codec::{Framed, FramedRead, FramedWrite, LengthDelimitedCodec};

use crate::file_format::{
    RawConfig, RawHeader, RawLatency, RawPing, RawPoint, RawResult, RawStream, RawStreamGroup,
};
use crate::plot::save_graph;
use crate::protocol::{
    codec, receive, send, ClientMessage, Hello, Ping, ServerMessage, TestStream,
};
use crate::serve::CountingCodec;

type Msg = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(PartialEq, Eq, Debug, Clone, Copy, PartialOrd, Ord)]
enum TestState {
    Setup,
    Grace1,
    LoadFromClient,
    Grace2,
    LoadFromServer,
    Grace3,
    LoadFromBoth,
    Grace4,
    End,
    EndPingRecv,
}

pub(crate) fn data() -> Vec<u8> {
    let mut vec = Vec::with_capacity(512 * 1024);
    let mut rng = StdRng::from_seed([
        18, 141, 186, 158, 195, 76, 244, 56, 219, 131, 65, 128, 250, 63, 228, 44, 233, 34, 9, 51,
        13, 72, 230, 131, 223, 240, 124, 77, 103, 238, 103, 186,
    ]);
    for _ in 0..vec.capacity() {
        vec.push(rng.gen())
    }
    vec
}

async fn hello<S: Sink<Bytes> + Stream<Item = Result<BytesMut, S::Error>> + Unpin>(
    stream: &mut S,
) -> Result<(), Box<dyn Error>>
where
    S::Error: Error + 'static,
{
    let hello = Hello::new();

    send(stream, &hello).await?;
    let server_hello: Hello = receive(stream).await?;

    if hello != server_hello {
        panic!(
            "Mismatched server hello, got {:?}, expected {:?}",
            server_hello, hello
        );
    }

    Ok(())
}

#[derive(Default)]
pub struct PlotConfig {
    pub split_bandwidth: bool,
    pub transferred: bool,
    pub width: Option<u64>,
    pub height: Option<u64>,
}

#[derive(Copy, Clone)]
pub struct Config {
    pub download: bool,
    pub upload: bool,
    pub both: bool,
    pub port: u16,
    pub load_duration: Duration,
    pub grace_duration: Duration,
    pub streams: u64,
    pub stream_stagger: Duration,
    pub ping_interval: Duration,
    pub bandwidth_interval: Duration,
}

async fn test_async(config: Config, server: &str, msg: Msg) -> Result<RawResult, Box<dyn Error>> {
    let control = net::TcpStream::connect((server, config.port)).await?;

    let server = control.peer_addr()?;

    msg(&format!("Connected to server {}", server));

    let mut control = Framed::new(control, codec());

    hello(&mut control).await?;

    send(&mut control, &ClientMessage::NewClient).await?;

    let setup_start = Instant::now();

    let reply: ServerMessage = receive(&mut control).await?;
    let id = match reply {
        ServerMessage::NewClient(Some(id)) => id,
        ServerMessage::NewClient(None) => return Err("Server was unable to create client".into()),
        _ => return Err(format!("Unexpected message {:?}", reply).into()),
    };

    let local_udp = if server.is_ipv6() {
        SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
    } else {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
    };

    let (latency, server_time_offset) = measure_latency(id, server, local_udp, setup_start).await?;

    msg(&format!(
        "Latency to server {:.2} ms",
        latency.as_secs_f64() * 1000.0
    ));

    let udp_socket = Arc::new(net::UdpSocket::bind(local_udp).await?);
    udp_socket.connect(server).await?;
    let udp_socket2 = udp_socket.clone();

    let data = Arc::new(data());

    let loading_streams: u32 = config.streams.try_into().unwrap();

    let grace = config.grace_duration;
    let load_duration = config.load_duration;
    let ping_interval = config.ping_interval;

    let loads = config.both as u32 + config.download as u32 + config.upload as u32;

    let estimated_duration = load_duration * loads + grace * 2;

    let (state_tx, state_rx) = watch::channel(TestState::Setup);

    if config.upload {
        upload_loaders(
            id,
            server,
            0,
            config,
            Duration::ZERO,
            data.clone(),
            state_rx.clone(),
            TestState::LoadFromClient,
        );
    }

    if config.both {
        upload_loaders(
            id,
            server,
            1,
            config,
            config.stream_stagger / 2,
            data.clone(),
            state_rx.clone(),
            TestState::LoadFromBoth,
        );
    }

    let download = config.download.then(|| {
        download_loaders(
            id,
            server,
            config,
            setup_start,
            state_rx.clone(),
            TestState::LoadFromServer,
        )
    });

    let both_download = config.both.then(|| {
        download_loaders(
            id,
            server,
            config,
            setup_start,
            state_rx.clone(),
            TestState::LoadFromBoth,
        )
    });

    send(&mut control, &ClientMessage::GetMeasurements).await?;

    let (rx, tx) = control.into_inner().into_split();
    let mut rx = FramedRead::new(rx, codec());
    let mut tx = FramedWrite::new(tx, codec());

    let upload_semaphore = Arc::new(Semaphore::new(0));
    let upload_semaphore_ = upload_semaphore.clone();
    let both_upload_semaphore = Arc::new(Semaphore::new(0));
    let both_upload_semaphore_ = both_upload_semaphore.clone();

    let bandwidth = tokio::spawn(async move {
        let mut bandwidth = Vec::new();

        loop {
            let reply: ServerMessage = receive(&mut rx).await.unwrap();
            match reply {
                ServerMessage::MeasureStreamDone { stream } => {
                    if stream.group == 0 {
                        &upload_semaphore_
                    } else {
                        &both_upload_semaphore_
                    }
                    .add_permits(1);
                }
                ServerMessage::Measure {
                    stream,
                    time,
                    bytes,
                } => {
                    bandwidth.push((stream, time, bytes));
                }
                ServerMessage::MeasurementsDone => break,
                _ => panic!("Unexpected message {:?}", reply),
            };
        }

        bandwidth
    });

    let ping_send = tokio::spawn(ping_send(
        id,
        state_rx.clone(),
        setup_start,
        udp_socket2.clone(),
        ping_interval,
        estimated_duration,
    ));

    let ping_recv = tokio::spawn(ping_recv(
        state_rx.clone(),
        setup_start,
        udp_socket2.clone(),
        ping_interval,
        estimated_duration,
    ));

    time::sleep(Duration::from_millis(100)).await;

    let start = Instant::now();

    state_tx.send(TestState::Grace1).unwrap();
    time::sleep(grace).await;

    if let Some((semaphore, _)) = download.as_ref() {
        state_tx.send(TestState::LoadFromServer).unwrap();
        msg(&format!("Testing download..."));
        let _ = semaphore.acquire_many(loading_streams).await.unwrap();

        state_tx.send(TestState::Grace2).unwrap();
        time::sleep(grace).await;
    }

    if config.upload {
        state_tx.send(TestState::LoadFromClient).unwrap();
        msg(&format!("Testing upload..."));
        let _ = upload_semaphore
            .acquire_many(loading_streams)
            .await
            .unwrap();

        state_tx.send(TestState::Grace3).unwrap();
        time::sleep(grace).await;
    }

    if let Some((semaphore, _)) = both_download.as_ref() {
        state_tx.send(TestState::LoadFromBoth).unwrap();
        msg(&format!("Testing both download and upload..."));
        let _ = semaphore.acquire_many(loading_streams).await.unwrap();
        let _ = both_upload_semaphore
            .acquire_many(loading_streams)
            .await
            .unwrap();

        state_tx.send(TestState::Grace4).unwrap();
        time::sleep(grace).await;
    }

    state_tx.send(TestState::End).unwrap();

    // Wait for pings to return
    time::sleep(Duration::from_millis(500)).await;
    state_tx.send(TestState::EndPingRecv).unwrap();

    let duration = start.elapsed();

    let pings_sent = ping_send.await?;
    send(&mut tx, &ClientMessage::Done).await?;

    let mut pings = ping_recv.await?;

    let bandwidth = bandwidth.await?;

    let download_bytes = wait_on_download_loaders(download).await;
    let both_download_bytes = wait_on_download_loaders(both_download).await;

    pings.sort_by_key(|d| d.0.index);
    let pings: Vec<_> = pings_sent
        .into_iter()
        .enumerate()
        .map(|(index, sent)| {
            let latency = pings
                .binary_search_by_key(&(index as u32), |e| e.0.index)
                .ok()
                .map(|ping| RawLatency {
                    total: pings[ping].1.saturating_sub(sent),
                    up: Duration::from_micros(pings[ping].0.time.wrapping_add(server_time_offset))
                        .saturating_sub(sent),
                });
            RawPing {
                index,
                sent,
                latency,
            }
        })
        .collect();

    let mut raw_streams = Vec::new();

    let to_raw = |data: &[(u64, u64)]| -> RawStream {
        RawStream {
            data: data
                .iter()
                .map(|&(time, bytes)| RawPoint {
                    time: Duration::from_micros(time),
                    bytes,
                })
                .collect(),
        }
    };

    let mut add_down = |both, data: &Option<Vec<Vec<(u64, u64)>>>| {
        data.as_ref().map(|download_bytes| {
            raw_streams.push(RawStreamGroup {
                download: true,
                both,
                streams: download_bytes.iter().map(|stream| to_raw(stream)).collect(),
            });
        });
    };

    add_down(false, &download_bytes);
    add_down(true, &both_download_bytes);

    let get_stream = |group, id| -> Vec<_> {
        bandwidth
            .iter()
            .filter(|e| e.0.group == group && e.0.id == id)
            .map(|e| (e.1, e.2))
            .collect()
    };

    let get_raw_upload_bytes = |group| -> Vec<RawStream> {
        (0..loading_streams)
            .map(|i| to_raw(&get_stream(group, i)))
            .collect()
    };

    config.upload.then(|| {
        raw_streams.push(RawStreamGroup {
            download: false,
            both: false,
            streams: get_raw_upload_bytes(0),
        })
    });

    config.upload.then(|| {
        raw_streams.push(RawStreamGroup {
            download: false,
            both: true,
            streams: get_raw_upload_bytes(1),
        })
    });

    let raw_config = RawConfig {
        stagger: config.stream_stagger,
        load_duration: config.load_duration,
        grace_duration: config.grace_duration,
        ping_interval: config.ping_interval,
        bandwidth_interval: config.bandwidth_interval,
    };

    let start = start.duration_since(setup_start);

    let raw_result = RawResult {
        version: RawHeader::default().version,
        generated_by: format!("Crusader {}", env!("CARGO_PKG_VERSION")),
        config: raw_config,
        ipv6: server.is_ipv6(),
        server_latency: latency,
        start,
        duration,
        stream_groups: raw_streams,
        pings,
    };

    Ok(raw_result)
}

async fn measure_latency(
    id: u64,
    server: SocketAddr,
    local_udp: SocketAddr,
    setup_start: Instant,
) -> Result<(Duration, u64), Box<dyn Error>> {
    let udp_socket = Arc::new(net::UdpSocket::bind(local_udp).await?);
    udp_socket.connect(server).await?;
    let udp_socket2 = udp_socket.clone();

    let samples = 50;

    let ping_send = tokio::spawn(ping_measure_send(id, setup_start, udp_socket, samples));

    let ping_recv = tokio::spawn(ping_measure_recv(setup_start, udp_socket2, samples));

    let (sent, recv) = join!(ping_send, ping_recv);

    let sent = sent.unwrap();
    let mut recv = recv.unwrap();

    recv.sort_by_key(|d| d.0.index);
    let mut pings: Vec<(Duration, Duration, u64)> = sent
        .into_iter()
        .enumerate()
        .filter_map(|(index, sent)| {
            recv.binary_search_by_key(&(index as u32), |e| e.0.index)
                .ok()
                .map(|ping| (sent, recv[ping].1 - sent, recv[ping].0.time))
        })
        .collect();
    pings.sort_by_key(|d| d.1);

    if pings.is_empty() {
        return Err("Unable to measure latency to server".into());
    }

    let (sent, latency, server_time) = pings[pings.len() / 2];

    let server_pong = sent + latency / 2;

    let server_offset = (server_pong.as_micros() as u64).wrapping_sub(server_time);

    Ok((latency, server_offset))
}

async fn ping_measure_send(
    id: u64,
    setup_start: Instant,
    socket: Arc<UdpSocket>,
    samples: u32,
) -> Vec<Duration> {
    let mut storage = Vec::with_capacity(samples as usize);
    let mut buf = [0; 64];

    let mut interval = time::interval(Duration::from_millis(10));

    for index in 0..samples {
        interval.tick().await;

        let current = setup_start.elapsed();

        let ping = Ping { id, time: 0, index };

        let mut cursor = Cursor::new(&mut buf[..]);
        bincode::serialize_into(&mut cursor, &ping).unwrap();
        let buf = &cursor.get_ref()[0..(cursor.position() as usize)];

        socket.send(buf).await.unwrap();

        storage.push(current);
    }

    storage
}

async fn ping_measure_recv(
    setup_start: Instant,
    socket: Arc<UdpSocket>,
    samples: u32,
) -> Vec<(Ping, Duration)> {
    let mut storage = Vec::with_capacity(samples as usize);
    let mut buf = [0; 64];

    let end = time::sleep(Duration::from_millis(10) * samples + Duration::from_millis(1000)).fuse();
    pin_mut!(end);

    loop {
        let result = {
            let packet = socket.recv(&mut buf).fuse();
            pin_mut!(packet);

            select! {
                result = packet => result,
                _ = end => break,
            }
        };

        let current = setup_start.elapsed();
        let len = result.unwrap();
        let buf = &mut buf[..len];
        let ping: Ping = bincode::deserialize(buf).unwrap();

        storage.push((ping, current));
    }

    storage
}

pub fn save_raw(result: &RawResult, name: &str) -> String {
    let name = unique(name, "crr");
    result.save(Path::new(&name));
    name
}

fn setup_loaders(
    id: u64,
    server: SocketAddr,
    count: u64,
) -> Vec<JoinHandle<Framed<TcpStream, LengthDelimitedCodec>>> {
    (0..count)
        .map(|_| {
            tokio::spawn(async move {
                let stream = TcpStream::connect(server)
                    .await
                    .expect("unable to bind TCP socket");
                let mut stream = Framed::new(stream, codec());
                hello(&mut stream).await.unwrap();
                send(&mut stream, &ClientMessage::Associate(id))
                    .await
                    .unwrap();

                stream
            })
        })
        .collect()
}

fn upload_loaders(
    id: u64,
    server: SocketAddr,
    group: u32,
    config: Config,
    stagger_offset: Duration,
    data: Arc<Vec<u8>>,
    state_rx: watch::Receiver<TestState>,
    state: TestState,
) {
    let loaders = setup_loaders(id, server, config.streams);

    for (i, loader) in loaders.into_iter().enumerate() {
        let mut state_rx = state_rx.clone();
        let data = data.clone();
        tokio::spawn(async move {
            let mut stream = loader.await.unwrap();

            wait_for_state(&mut state_rx, state).await;

            time::sleep(config.stream_stagger * i as u32 + stagger_offset).await;

            let stopping = Instant::now() + config.load_duration;

            send(
                &mut stream,
                &ClientMessage::LoadFromClient {
                    stream: TestStream {
                        group,
                        id: i as u32,
                    },
                    bandwidth_interval: config.bandwidth_interval.as_micros() as u64,
                },
            )
            .await
            .unwrap();

            let mut raw = stream.into_inner();

            loop {
                if Instant::now() >= stopping {
                    break;
                }

                raw.write_all(data.as_ref()).await.unwrap();

                yield_now().await;
            }
        });
    }
}

async fn wait_on_download_loaders(
    download: Option<(Arc<Semaphore>, Vec<JoinHandle<Vec<(u64, u64)>>>)>,
) -> Option<Vec<Vec<(u64, u64)>>> {
    match download {
        Some((_, result)) => {
            let bytes: Vec<_> = stream::iter(result)
                .then(|data| async move { data.await.unwrap() })
                .collect()
                .await;
            Some(bytes)
        }
        None => None,
    }
}

fn download_loaders(
    id: u64,
    server: SocketAddr,
    config: Config,
    setup_start: Instant,
    state_rx: watch::Receiver<TestState>,
    state: TestState,
) -> (Arc<Semaphore>, Vec<JoinHandle<Vec<(u64, u64)>>>) {
    let semaphore = Arc::new(Semaphore::new(0));
    let loaders = setup_loaders(id, server, config.streams);

    let loaders = loaders
        .into_iter()
        .enumerate()
        .map(|(i, loader)| {
            let mut state_rx = state_rx.clone();
            let semaphore = semaphore.clone();

            tokio::spawn(async move {
                let stream = loader.await.unwrap();

                let (rx, tx) = stream.into_inner().into_split();
                let mut tx = FramedWrite::new(tx, codec());
                let mut rx = FramedRead::with_capacity(rx, CountingCodec, 512 * 1024);

                wait_for_state(&mut state_rx, state).await;

                time::sleep(config.stream_stagger * i as u32).await;

                send(&mut tx, &ClientMessage::LoadFromServer).await.unwrap();

                tokio::spawn(async move {
                    time::sleep(config.load_duration).await;

                    send(&mut tx, &ClientMessage::Done).await.unwrap();
                });

                let bytes = Arc::new(AtomicU64::new(0));
                let bytes_ = bytes.clone();

                let done = Arc::new(AtomicBool::new(false));
                let done_ = done.clone();

                let measures = tokio::spawn(async move {
                    let mut measures = Vec::new();
                    let mut interval = time::interval(config.bandwidth_interval);
                    loop {
                        interval.tick().await;

                        let current_time = Instant::now();
                        let current_bytes = bytes_.load(Ordering::Acquire);

                        measures.push((
                            current_time.duration_since(setup_start).as_micros() as u64,
                            current_bytes,
                        ));

                        if done_.load(Ordering::Acquire) {
                            break;
                        }
                    }
                    measures
                });

                while let Some(size) = rx.next().await {
                    let size = size.unwrap();
                    bytes.fetch_add(size as u64, Ordering::Release);
                    yield_now().await;
                }

                done.store(true, Ordering::Release);

                semaphore.add_permits(1);

                measures.await.unwrap()
            })
        })
        .collect();
    (semaphore, loaders)
}

async fn wait_for_state(state_rx: &mut watch::Receiver<TestState>, state: TestState) {
    loop {
        if *state_rx.borrow_and_update() == state {
            break;
        }
        state_rx.changed().await.unwrap();
    }
}

async fn ping_send(
    id: u64,
    state_rx: watch::Receiver<TestState>,
    setup_start: Instant,
    socket: Arc<UdpSocket>,
    interval: Duration,
    estimated_duration: Duration,
) -> Vec<Duration> {
    let mut storage = Vec::with_capacity(
        ((estimated_duration.as_secs_f64() + 2.0) * (1000.0 / interval.as_millis() as f64) * 1.5)
            as usize,
    );
    let mut buf = [0; 64];

    let mut interval = time::interval(interval);

    loop {
        interval.tick().await;

        if *state_rx.borrow() >= TestState::End {
            break;
        }

        let index = storage.len().try_into().unwrap();

        let current = setup_start.elapsed();

        let ping = Ping { id, time: 0, index };

        let mut cursor = Cursor::new(&mut buf[..]);
        bincode::serialize_into(&mut cursor, &ping).unwrap();
        let buf = &cursor.get_ref()[0..(cursor.position() as usize)];

        socket.send(buf).await.expect("unable to udp ping");

        storage.push(current);
    }

    storage
}

async fn ping_recv(
    mut state_rx: watch::Receiver<TestState>,
    setup_start: Instant,
    socket: Arc<UdpSocket>,
    interval: Duration,
    estimated_duration: Duration,
) -> Vec<(Ping, Duration)> {
    let mut storage = Vec::with_capacity(
        ((estimated_duration.as_secs_f64() + 2.0) * (1000.0 / interval.as_millis() as f64) * 1.5)
            as usize,
    );
    let mut buf = [0; 64];

    let end = wait_for_state(&mut state_rx, TestState::EndPingRecv).fuse();
    pin_mut!(end);

    loop {
        let result = {
            let packet = socket.recv(&mut buf).fuse();
            pin_mut!(packet);

            select! {
                result = packet => result,
                _ = end => break,
            }
        };

        let current = setup_start.elapsed();
        let len = result.unwrap();
        let buf = &mut buf[..len];
        let ping: Ping = bincode::deserialize(buf).unwrap();

        storage.push((ping, current));
    }

    storage
}

pub fn timed(name: &str) -> String {
    let time = chrono::Local::now().format(" %Y.%m.%d %H-%M-%S");
    format!("{}{}", name, time)
}

pub(crate) fn unique(name: &str, ext: &str) -> String {
    let stem = timed(name);
    let mut i: usize = 0;
    loop {
        let file = if i != 0 {
            format!("{} {}", stem, i)
        } else {
            stem.to_string()
        };
        let file = format!("{}.{}", file, ext);
        if !Path::new(&file).exists() {
            return file;
        }
        i += 1;
    }
}

pub fn test(config: Config, plot: PlotConfig, host: &str) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt
        .block_on(test_async(config, host, Arc::new(|msg| println!("{msg}"))))
        .unwrap();
    println!("Writing data...");
    let raw = save_raw(&result, "data");
    println!("Saved raw data as {}", raw);
    let file = save_graph(&plot, &result.to_test_result(), "plot");
    println!("Saved plot as {}", file);
}

pub fn test_callback(
    config: Config,
    host: &str,
    msg: Arc<dyn Fn(&str) + Send + Sync>,
    done: Box<dyn FnOnce(Option<Result<RawResult, String>>) + Send>,
) -> oneshot::Sender<()> {
    let (tx, rx) = oneshot::channel();
    let host = host.to_string();
    thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();

        done(rt.block_on(async move {
            let mut result = task::spawn(async move {
                test_async(config, &host, msg)
                    .await
                    .map_err(|error| error.to_string())
            })
            .fuse();

            select! {
                result = result => {
                    Some(result.map_err(|error| error.to_string()).and_then(|result| result))
                },
                result = rx.fuse() => {
                    result.unwrap();
                    None
                },
            }
        }));
    });
    tx
}
