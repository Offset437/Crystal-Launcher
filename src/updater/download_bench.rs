//! Opt-in public-file timing and staging. Reads a saved manifest and downloads
//! a bounded sample into memory or a new scratch directory. No install writes
//! or account sign-ins.
use super::*;

fn download_sample() -> Arc<Vec<FileHistory>> {
    let root = PathBuf::from(std::env::var_os("CRYSTAL_DOWNLOAD_BENCH_INSTALL")
        .expect("Set CRYSTAL_DOWNLOAD_BENCH_INSTALL to the installation whose manifest is read"));
    assert!(root.is_absolute());
    ensure_plain(&root).unwrap();
    let m = manifest::parse(&fs::read_to_string(confined(&root, "patch.cfg").unwrap()).unwrap()).unwrap();
    validate_manifest(&m).unwrap();
    let bounded = |h: &&FileHistory| h.current().sig.len <= 8 * 1024 * 1024;
    let small: Vec<_> = m.files.iter().filter(bounded)
        .filter(|h| (16 * 1024..=polp::CHUNK_LEN as u64).contains(&h.current().direct_len)).collect();
    let large: Vec<_> = m.files.iter().filter(bounded)
        .filter(|h| (256 * 1024..=512 * 1024).contains(&h.current().direct_len)).collect();
    let pairs: usize = std::env::var("CRYSTAL_DOWNLOAD_BENCH_PAIRS").unwrap_or_else(|_| "12".into())
        .parse().expect("sample pairs must be an integer");
    assert!((12..=48).contains(&pairs) && small.len() >= pairs && large.len() >= pairs);
    let mut selected = Vec::new();
    for index in 0..pairs {
        selected.push(small[index * small.len() / pairs].clone());
        selected.push(large[index * large.len() / pairs].clone());
    }
    Arc::new(selected)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "read-only official download timing; requires CRYSTAL_DOWNLOAD_BENCH_INSTALL with a saved patch.cfg"]
async fn official_download_concurrency_and_nodelay() {
    let selected = download_sample();
    let total: u64 = selected.iter().map(|h| h.current().direct_len).sum();
    let requests: u64 = selected.iter().map(|h| h.current().direct_len.div_ceil(polp::CHUNK_LEN as u64)).sum();
    let per_worker_bound = selected.iter().map(|h| h.current().direct_len + h.current().sig.len).max().unwrap();
    assert!(per_worker_bound * 12 <= 256 * 1024 * 1024);
    eprintln!("download sample files={} encodedBytes={total} requiredChunks={requests} maximumPayloadBytesPerWorker={per_worker_bound}", selected.len());
    let configs: Vec<_> = std::env::var("CRYSTAL_DOWNLOAD_BENCH_CONFIGS")
        .unwrap_or_else(|_| "6:0,8:0,12:0,6:0,6:1,6:0".into()).split(',').map(|config| {
            let (workers, setting) = config.split_once(':').expect("config must be workers:nodelay (0 or 1)");
            let workers: usize = workers.parse().expect("workers must be an integer");
            assert!((1..=12).contains(&workers));
            let nodelay = match setting { "0" => false, "1" => true, _ => panic!("nodelay must be 0 or 1") };
            (workers, nodelay)
        }).collect();
    assert!(!configs.is_empty() && configs.len() <= 8);
    for (workers, nodelay) in configs {
        let started = Instant::now();
        let next = Arc::new(AtomicUsize::new(0));
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..workers {
            let selected = selected.clone();
            let next = next.clone();
            tasks.spawn(async move {
                let op = Operation::new();
                let connect_started = Instant::now();
                let mut session = Session::connect(&op).await?;
                session.stream.set_nodelay(nodelay).map_err(|e| e.to_string())?;
                let connection = connect_started.elapsed();
                let peer = session.stream.peer_addr().map_err(|e| e.to_string())?;
                let mut bytes = 0;
                let mut files = 0;
                let mut transfers = Duration::ZERO;
                let mut processing = Duration::ZERO;
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(history) = selected.get(index) else { break; };
                    let version = history.current();
                    let transfer_started = Instant::now();
                    let raw = session.fetch(&version.direct, version.direct_len, &op).await?;
                    transfers += transfer_started.elapsed();
                    // A retry creates a new socket; detect it rather than
                    // attributing a mixed configuration to TCP_NODELAY.
                    if session.stream.nodelay().map_err(|e| e.to_string())? != nodelay {
                        return Err("Download benchmark reconnected with a different socket setting".into());
                    }
                    let process_started = Instant::now();
                    let decoded = decode_payload(&raw, version.sig.len as usize)?;
                    if FileSig::of(&decoded) != version.sig {
                        return Err(format!("{}: benchmark canonical signature mismatch", history.path));
                    }
                    processing += process_started.elapsed();
                    bytes += raw.len() as u64;
                    files += 1;
                }
                Ok::<_, String>((bytes, files, connection, transfers, processing, peer))
            });
        }
        let mut bytes = 0;
        let mut files = 0;
        let mut connections = Duration::ZERO;
        let mut transfers = Duration::ZERO;
        let mut processing = Duration::ZERO;
        let mut peers = HashSet::new();
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok((n, count, connection, transfer, process, peer))) => {
                    bytes += n;
                    files += count;
                    connections += connection;
                    transfers += transfer;
                    processing += process;
                    peers.insert(peer);
                }
                failure => {
                    // There are no detached disk jobs or installation writes.
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                    panic!("Download benchmark workers={workers} nodelay={nodelay} failed: {failure:?}");
                }
            }
        }
        assert_eq!(bytes, total);
        assert_eq!(files, selected.len());
        let elapsed = started.elapsed().as_secs_f64();
        eprintln!("download benchmark workers={workers} nodelay={nodelay} elapsed={elapsed:.3}s encodedBytes={bytes} MiBps={:.3} files={files} summedConnect={:.3}s summedTransfer={:.3}s summedDecodeHash={:.3}s peers={peers:?}",
            bytes as f64 / (1024.0 * 1024.0) / elapsed, connections.as_secs_f64(), transfers.as_secs_f64(), processing.as_secs_f64());
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "official staging into new scratch files; requires CRYSTAL_DOWNLOAD_BENCH_INSTALL with a saved patch.cfg"]
async fn official_download_staging_and_cache_resume() {
    let selected = download_sample();
    let manifest = Manifest { files: selected.as_ref().clone() };
    validate_manifest(&manifest).unwrap();
    let temp = std::env::temp_dir();
    assert!(temp.is_absolute());
    ensure_plain(&temp).unwrap();
    let root = temp.join(format!("crystal-stage-live-{}-{}", std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
    assert!(root.starts_with(&temp) && root != temp);
    fs::create_dir(&root).unwrap();
    let staged = root.join("staged");
    fs::create_dir(&staged).unwrap();
    let planned: Vec<_> = (0..manifest.files.len()).map(|index|
        Planned { index, action: Action::Direct, original: None }).collect();
    let encoded: u64 = manifest.files.iter().map(|h| h.current().direct_len).sum();
    let decoded: u64 = manifest.files.iter().map(|h| h.current().sig.len).sum();
    let op = Operation::new();
    let started = Instant::now();
    let (ready, bytes) = stage_plan(&root, &staged, &manifest, &planned, &op).await.unwrap();
    let elapsed = started.elapsed();
    assert_eq!(bytes, decoded);
    assert_eq!(ready.len(), planned.len());
    assert_eq!((op.snapshot().done, op.snapshot().total), (encoded, encoded));
    for (index, (path, cache, sig, original)) in ready.iter().enumerate() {
        assert_eq!(path, &manifest.files[index].path);
        assert_eq!(*sig, manifest.files[index].current().sig);
        assert_eq!(*sig, signature(cache, &op).unwrap());
        assert_eq!(*original, None);
        assert!(!root.join(path).exists());
    }
    let resumed = Operation::new();
    let resume_started = Instant::now();
    let (reused, resume_bytes) = stage_plan(&root, &staged, &manifest, &planned, &resumed).await.unwrap();
    let resume_elapsed = resume_started.elapsed();
    assert_eq!(resume_bytes, 0);
    assert_eq!(reused, ready);
    assert_eq!((resumed.snapshot().done, resumed.snapshot().total), (encoded, encoded));
    assert!(!root.join("patch.ver").exists());
    assert!(!root.join("patch.cfg").exists());
    eprintln!("production staging workers={MAX_DOWNLOAD_WORKERS} files={} encodedBytes={encoded} decodedBytes={bytes} elapsed={:.3}s; verified cache resume elapsed={:.3}s newlyStagedBytes={resume_bytes}",
        ready.len(), elapsed.as_secs_f64(), resume_elapsed.as_secs_f64());
    ensure_plain(&root).unwrap();
    fs::remove_dir_all(&root).unwrap();
}
