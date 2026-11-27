//! Real compute + runner protocol, with a disposable Git store and RESP cache.
use super::*;
use std::io::Read;
use std::net::TcpListener;
use std::sync::{atomic::AtomicBool, Arc};
use std::time::Instant;

const SENTINEL: &str =
    "unforged@sha256:756e666f726765643a63616f732d72756e6e65723a76312e3100000000000000";
fn digest(c: char) -> String {
    format!("native@sha256:{}", c.to_string().repeat(64))
}

struct Fixture {
    config: Config,
    dir: PathBuf,
    request: String,
    required: std::collections::BTreeMap<String, String>,
    stop: Arc<AtomicBool>,
    cache_thread: Option<std::thread::JoinHandle<()>>,
}
impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "caos-native-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        assert!(Command::new("git")
            .args(["init", "--bare"])
            .arg(&dir)
            .output()
            .unwrap()
            .status
            .success());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let cache_thread = std::thread::spawn(move || {
            let mut values = HashMap::<String, String>::new();
            while !done.load(Ordering::SeqCst) {
                let (stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(e) => panic!("accept: {e}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let count: usize = line.trim().strip_prefix('*').unwrap().parse().unwrap();
                let mut args = Vec::new();
                for _ in 0..count {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    let len: usize = line.trim().strip_prefix('$').unwrap().parse().unwrap();
                    let mut bytes = vec![0; len + 2];
                    reader.read_exact(&mut bytes).unwrap();
                    args.push(String::from_utf8(bytes[..len].to_vec()).unwrap());
                }
                let reply = match args[0].as_str() {
                    "GET" => match values.get(&args[1]) {
                        Some(v) => format!("${}\r\n{v}\r\n", v.len()),
                        None => "$-1\r\n".into(),
                    },
                    "SET" => {
                        values.insert(args[1].clone(), args[2].clone());
                        "+OK\r\n".into()
                    }
                    "RPUSH" | "HSET" => ":1\r\n".into(),
                    "HGET" => "$-1\r\n".into(),
                    other => panic!("unexpected cache command {other}"),
                };
                reader.get_mut().write_all(reply.as_bytes()).unwrap();
            }
        });
        let repo = gix::open(&dir).unwrap();
        let config = Config {
            registry_push_url: "http://127.0.0.1:1".into(),
            registry_pull_host: "unused".into(),
            redis_addr: addr,
            cache_namespace: dir.display().to_string(),
            git_dir: dir.display().to_string(),
            secrets_git: dir.display().to_string(),
            repo: repo.into_sync(),
        };
        let base = store_git_blob(&config, SENTINEL.as_bytes()).unwrap();
        let unique = store_git_blob(&config, dir.to_str().unwrap().as_bytes()).unwrap();
        let request = store_git_tree(
            &config,
            vec![
                named_entry("base", gix::objs::tree::EntryKind::Blob.into(), base),
                named_entry(
                    "required-native-test",
                    gix::objs::tree::EntryKind::Blob.into(),
                    unique,
                ),
            ],
        )
        .unwrap()
        .to_string();
        let required = args_entries(&config, &request).unwrap();
        Self {
            config,
            dir,
            request,
            required,
            stop,
            cache_thread: Some(cache_thread),
        }
    }
    fn call(&self) -> Result<RunResponse, HttpError> {
        run(
            &self.config,
            &format!("req={}", self.request),
            &crate::secrets::Context::default(),
        )
    }
    fn worker(
        &self,
        image: Option<String>,
        release: Option<mpsc::Receiver<()>>,
    ) -> std::thread::JoinHandle<()> {
        let required = self.required.clone();
        let result = format!(
            "blob {}",
            store_git_blob(&self.config, b"native worker output").unwrap()
        );
        std::thread::spawn(move || {
            let reply = crate::runner::poll(
                None,
                &serde_json::json!({"required": required, "worker_image": image, "ttl_ms": 5000})
                    .to_string(),
            )
            .unwrap();
            let reply: serde_json::Value = serde_json::from_slice(&reply).unwrap();
            if reply["idle"] == true {
                return;
            }
            let job = &reply["job"];
            assert_eq!(job["image_ref"], image.as_deref().unwrap());
            if let Some(release) = release {
                release.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            crate::runner::result(None, &serde_json::json!({"req": job["req"], "nonce": job["nonce"], "ok": true, "result": result, "worker_image": image}).to_string()).unwrap();
        })
    }
    fn ready(&self, image: &str) {
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            if crate::runner::native_identity(&self.required)
                .ok()
                .as_deref()
                == Some(image)
            {
                break;
            }
            assert!(Instant::now() < until, "worker did not advertise identity");
            std::thread::yield_now();
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.cache_thread.take().unwrap().join().unwrap();
        std::fs::remove_dir_all(&self.dir).unwrap();
    }
}
#[test]
fn native_identity_rejected_tenure_poll_does_not_change_binding() {
    let fixture = Fixture::new();
    let good = digest('a');
    let worker = fixture.worker(Some(good.clone()), None);
    fixture.ready(&good);
    fixture.call().unwrap();
    worker.join().unwrap();
    let error = crate::runner::poll(
        None,
        &serde_json::json!({
            "required": fixture.required, "worker_image": digest('f'),
            "tenure": "already-lapsed-tenure", "ttl_ms": 10
        })
        .to_string(),
    )
    .unwrap_err();
    assert_eq!(error.status(), 410);
    let (_, selected) = bind_native_request(&fixture.config, &fixture.request, SENTINEL)
        .unwrap()
        .unwrap();
    assert_eq!(selected, good);
    assert_receipt(&fixture.call().unwrap(), "hit", &good);
}

fn assert_receipt(answer: &RunResponse, cache: &str, image: &str) {
    let wire = crate::run_http_response(RunResponse {
        body: answer.body.clone(),
        headers: answer.headers.clone(),
    });
    assert_eq!(wire.status_code().0, 200);
    for (name, expected) in [(CACHE_HEADER, cache), (IMAGE_HEADER, image)] {
        assert_eq!(
            wire.headers()
                .iter()
                .find(|h| h.field.equiv(name))
                .map(|h| h.value.as_str()),
            Some(expected)
        );
    }
    assert_ne!(image, SENTINEL);
}
#[test]
fn native_identity_receipt_cold_reports_worker_digest_not_sentinel() {
    let fixture = Fixture::new();
    let image = digest('a');
    let worker = fixture.worker(Some(image.clone()), None);
    fixture.ready(&image);
    let answer = fixture.call().unwrap();
    worker.join().unwrap();
    assert_receipt(&answer, "miss", &image);
}
#[test]
fn native_identity_warm_hit_reports_producer_digest() {
    let fixture = Fixture::new();
    let image = digest('b');
    let worker = fixture.worker(Some(image.clone()), None);
    fixture.ready(&image);
    let cold = fixture.call().unwrap();
    worker.join().unwrap();
    let warm = fixture.call().unwrap();
    assert_eq!(cold.body, warm.body);
    assert_receipt(&warm, "hit", &image);
}
#[test]
fn native_identity_changed_binary_misses_cache() {
    let fixture = Fixture::new();
    for image in [digest('c'), digest('d')] {
        let worker = fixture.worker(Some(image.clone()), None);
        fixture.ready(&image);
        let answer = fixture.call().unwrap();
        worker.join().unwrap();
        assert_receipt(&answer, "miss", &image);
    }
}
#[test]
fn native_identity_unknown_worker_refused_not_sentinel_fallback() {
    let fixture = Fixture::new();
    let poll = serde_json::json!({"required": fixture.required, "ttl_ms": 10});
    crate::runner::poll(None, &poll.to_string()).unwrap();
    let error = fixture.call().err().expect("unknown native worker refused");
    assert_eq!(error.status(), 503);
    assert!(error.message().contains("sentinel fallback"));
    assert!(resolve_image_endpoint(&fixture.config, &format!("image={SENTINEL}")).is_err());
}
#[test]
fn native_identity_concurrent_identical_calls_one_executed_one_hit() {
    let fixture = Fixture::new();
    let image = digest('e');
    let (release, wait) = mpsc::channel();
    let worker = fixture.worker(Some(image.clone()), Some(wait));
    fixture.ready(&image);
    std::thread::scope(|scope| {
        let first = scope.spawn(|| fixture.call().unwrap());
        let second = scope.spawn(|| fixture.call().unwrap());
        // Wait for the production single-flight table to contain the waiter.
        let (bound, _) = bind_native_request(&fixture.config, &fixture.request, SENTINEL)
            .unwrap()
            .unwrap();
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            let joined = flights()
                .lock()
                .unwrap()
                .get(&bound)
                .is_some_and(|flight| !flight.waiters.is_empty());
            if joined {
                break;
            }
            assert!(Instant::now() < until, "second call did not join flight");
            std::thread::yield_now();
        }
        release.send(()).unwrap();
        let answers = [first.join().unwrap(), second.join().unwrap()];
        let mut origins: Vec<_> = answers
            .iter()
            .map(|answer| {
                answer
                    .headers
                    .iter()
                    .find(|(name, _)| *name == CACHE_HEADER)
                    .unwrap()
                    .1
                    .as_str()
            })
            .collect();
        origins.sort();
        assert_eq!(origins, ["hit", "miss"]);
        for answer in &answers {
            assert_receipt(answer, &answer.headers[0].1, &image);
        }
        assert_eq!(answers[0].body, answers[1].body);
    });
    worker.join().unwrap();
}
