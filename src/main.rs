use std::env;
use std::process::exit;
use std::sync::Arc;
use std::time::{Duration, Instant};
use atomic_counter::AtomicCounter;
use lazy_static::lazy_static;
use regex::Regex;
use reqwest::{Proxy, StatusCode};
use reqwest::redirect::Policy;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::{mpsc};
use tokio::task;
use tokio::task::{JoinSet};
use tokio::time::sleep;
use serde::Deserialize;

#[derive(Deserialize)]
struct AlbumImages {
    count: usize,
}

#[derive(Deserialize)]
struct Album {
    title: Option<String>,
    description: Option<String>,
    album_images: Option<AlbumImages>,
}

const NO_PROXY_CONC_LIMIT: usize = 6;

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 4 && args.len() != 5 {
        println!("Usage: imgur_a <links_file> <output> <concurrent> [<proxies=--no-proxies>]");
        println!("\tlinks_file: Path to the file with ids to work off, one id per line (just the aBcDe id, not a link)");
        println!("\toutput: Path to the output file, will be appended to");
        println!("\tconcurrent: How many requests to queue per second max. (actual rate will be slightly lower). If not using proxies this is limited to {}", NO_PROXY_CONC_LIMIT);
        println!("\tproxies: Proxy list file or --no-proxies (default) to not use proxies");
        println!("\t         Proxy list file has the format of 'PROXY_HOST:PROXY_PORT:PROXY_USER:PROXY_PASSWORD' with one entry per line");
        println!("\t         So for example 'proxy.example.com:1234:username:password123'");
        println!("\t         For each entry, one worker will be spawned.");
        std::process::exit(1);
    }
    let mut proxies = vec![];
    let concurrent: usize = args[3].parse().unwrap();
    let proxy_file_name = if args.len() == 4 { "--no-proxies" } else { args[4].as_str()};
    if proxy_file_name == "--no-proxies" {
        if concurrent > NO_PROXY_CONC_LIMIT {
            println!("Concurrency seems to be set too high for a single ip. (max. {NO_PROXY_CONC_LIMIT}), refusing to start");
            exit(1);
        }
        for _ in 0..concurrent {
            proxies.push(Proxy::custom(|_| -> Option<&'static str> { None }));
        }
    } else {
        // read proxies:
        match File::open(args[3].as_str()).await {
            Ok(file) => {
                let reader = BufReader::new(file);

                let mut lines = reader.lines();

                loop {
                    match lines.next_line().await {
                        Ok(l) => {
                            if let Some(line) = l {
                                if line.is_empty() {
                                    continue;
                                }
                                let mut splits: Vec<&str> = line.as_str().split(":").collect();
                                if splits.len() < 2 {
                                    println!("Proxy line \"{}\" was malformed", line);
                                }
                                while splits.len() < 4 {
                                    splits.push("");
                                }
                                let purl = format!("http://{}:{}@{}:{}/", splits[2], splits[3], splits[0], splits[1]);
                                match Proxy::all(purl.as_str()) {
                                    Ok(proxy) => {
                                        proxies.push(proxy)
                                    }
                                    Err(e) => {
                                        println!("Bad proxy line '{}' -> '{}': {}", line, purl, e);
                                        std::process::exit(1);
                                    }
                                }
                            } else {
                                break;
                            }
                        }
                        Err(e) => {
                            println!("Failed to read line from proxy file '{}': {}", args[4], e);
                            std::process::exit(1);
                        }
                    }
                }
            }
            Err(e) => {
                println!("Failed to open file '{}': {}", args[4], e);
                std::process::exit(1);
            }
        }
    }
    //
    let links_file = Arc::new(args[1].clone());
    let out_file = Arc::new(args[2].clone());
    let (producer, consumer) = async_channel::bounded(concurrent + 2);
    let producer = Arc::new(producer);
    let consumer = Arc::new(consumer);
    let (requeue_tx, mut requeue_rx) = mpsc::channel(concurrent * 10);
    let requeue_tx = Arc::new(requeue_tx);
    let mut tasks = JoinSet::<()>::new();
    {
        let producer = producer.clone();
        let links_file = links_file.clone();
        tasks.spawn(async move {
            match File::open(links_file.as_str()).await {
                Ok(file) => {
                    let reader = BufReader::new(file);

                    let mut lines = reader.lines();
                    let mut dispatched = 0;
                    loop {
                        loop {
                            match requeue_rx.try_recv() {
                                Ok(line) => {
                                    match producer.send(line).await {
                                        Ok(_) => {}
                                        Err(e) => {
                                            println!("Failed to send task {}", e);
                                            producer.close();
                                            return;
                                        }
                                    }
                                    dispatched += 1;
                                }
                                Err(_) => {
                                    break;
                                }
                            }
                        }
                        match lines.next_line().await {
                            Ok(l) => {
                                if let Some(line) = l {
                                    match producer.send(line).await {
                                        Ok(_) => {}
                                        Err(e) => {
                                            println!("Failed to send task {}", e);
                                            producer.close();
                                            return;
                                        }
                                    }
                                    dispatched += 1;
                                    while dispatched >= concurrent {
                                        dispatched -= concurrent;
                                        sleep(Duration::from_secs(1)).await;
                                    }
                                } else {
                                    println!("Producer is done.");
                                    producer.close();
                                    return;
                                }
                            }
                            Err(e) => {
                                println!("Failed to read line from file '{}': {}", links_file, e);
                                producer.close();
                            }
                        }
                    }
                }
                Err(e) => {
                    println!("Failed to open file '{}': {}", links_file, e);
                    producer.close();
                }
            }
            println!("Producer is done.");
            producer.close();
        });
    }
    let (done_tx, mut done_rx) = mpsc::channel(concurrent * 10);
    let done_tx = Arc::new(done_tx);
    let tasks_worked = Arc::new(atomic_counter::RelaxedCounter::new(0));
    let tasks_found = Arc::new(atomic_counter::RelaxedCounter::new(0));
    let tasks_failed = Arc::new(atomic_counter::RelaxedCounter::new(0));
    let workers_failed = Arc::new(atomic_counter::ConsistentCounter::new(0));
    let start = Instant::now();
    let mut worker_counter = 0;
    for proxy in proxies {
        let tasks_worked = tasks_worked.clone();
        let tasks_found = tasks_found.clone();
        let tasks_failed = tasks_failed.clone();
        let consumer = consumer.clone();
        let done_tx = done_tx.clone();
        let requeue_tx = requeue_tx.clone();
        let workers_failed = workers_failed.clone();
        worker_counter += 1;
        let worker_i = worker_counter;
        tasks.spawn(async move {
            // slowly ramp up workers so we don't spam everything at once at the start
            let r = worker_i as f32 / concurrent as f32 * 10000.0;
            sleep(std::time::Duration::from_millis(r as u64)).await;
            const STARTING_COOKIES: &'static str = "retina=0; over18=1; m_section=hot; m_sort=time; is_emerald=0; is_authed=0; frontpagebeta=0; postpagebeta=0;";
            let client = reqwest::Client::builder()
                .gzip(true)
                .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/113.0.0.0 Safari/537.36")
                .proxy(proxy.clone())
                .connect_timeout(Duration::from_secs(20))
                .timeout(Duration::from_secs(20))
                .redirect(Policy::none())
                .build();

            if client.is_err() {
                println!("Failed to build http client: {}", client.unwrap_err());
                sleep(Duration::from_millis(500)).await;
                return;
            }
            let client = client.unwrap();
            let mut referer = String::from("https://imgur.com/");
            //let cookie_url = "https://imgur.com".parse().unwrap();
            loop {
                match consumer.recv().await {
                    Ok(task) => {
                        let url = format!("https://imgur.com/a/{}", task);
                        let mut success = false;
                        for _ in 0..10 {
                            let req = client.get(url.as_str())
                                .header("Referer", referer.as_str())
                                .header("Cookie", STARTING_COOKIES);
                            // make sure the cookies are always right
                            //cookie_store.add_cookie_str(STARTING_COOKIES, &cookie_url);
                            match req
                                .send()
                                .await {
                                Ok(res) => {
                                    //println!("{}: {}", url, res.status());
                                    let worked = tasks_worked.inc() + 1;
                                    let status = res.status();
                                    let mut result = None;
                                    if status.is_success() {
                                        match res.text().await {
                                            Ok(body) => {
                                                lazy_static! {
                                                    static ref RE: Regex = Regex::new("(?msi)<script.*image\\s*:\\s?(\\{.*?\\}),\r?\n.*?</script>").unwrap();
                                                }
                                                if let Some(json_match) = RE.captures(body.as_str()) {
                                                    let d = json_match.get(1).unwrap().as_str();
                                                    match serde_json::from_str::<Album>(d) {
                                                        Ok(data) => {
                                                            if (data.album_images.is_some() && data.album_images.unwrap().count > 0) || !data.title.unwrap_or(String::new()).is_empty() || !data.description.unwrap_or(String::new()).is_empty() {
                                                                result = Some(d.to_string());
                                                            } else {
                                                                result = None;
                                                            }
                                                        }
                                                        Err(e) => {
                                                            println!("Failed to parse response json for {}: {}. Json: {}", url, e, d);
                                                        }
                                                    }
                                                } else {
                                                    println!("Failed to find album data in response for {}:\n{}", url, body);
                                                }
                                            }
                                            Err(e) => {
                                                println!("Failed to read response body for {}: {}", url, e);
                                            }
                                        }
                                    } else {
                                        if status.is_server_error() {
                                            let body = res.text().await;
                                            let body_str;
                                            if body.is_err() {
                                                body_str = format!("ERR {}", body.unwrap_err());
                                            } else {
                                                body_str = body.unwrap();
                                            }
                                            let mut bs = body_str.as_str();
                                            if bs.len() > 100 {
                                                bs = &bs[0..100];
                                            }
                                            println!("Failed to request '{}', got status {}, retrying in 30s (body: {})", url, status.as_u16(), bs);
                                            sleep(Duration::from_secs(30)).await;
                                            continue;
                                        } else if status == StatusCode::TOO_MANY_REQUESTS {
                                            println!("Worker #{} got 429'd", worker_i);
                                            break;
                                        } else if status != StatusCode::NOT_FOUND && status.as_u16() != 403 {
                                            println!("Failed to request '{}', got status {}, retrying in 30s", url, status.as_u16());
                                            sleep(Duration::from_secs(30)).await;
                                            continue;
                                        } else {
                                            result = None;
                                        }
                                    }
                                    success = true;
                                    if let Some(body) = result {
                                        tasks_found.inc();
                                        match done_tx.send(body).await {
                                            Ok(_) => {}
                                            Err(e) => {
                                                println!("Failed to send result {}", e);
                                                return;
                                            }
                                        }
                                    } else {
                                        tasks_failed.inc();
                                    }
                                    if worked % concurrent == 0 {
                                        let found = tasks_found.get();
                                        let failed = tasks_failed.get();
                                        let elapsed = start.elapsed();
                                        println!("Worked {:07}, found {:07}, failed {:07}, ~{:.1}% exist, {:.1} req/s", worked, found, failed, found as f32 / worked as f32 * 100.0, worked as f32 / elapsed.as_secs_f32());
                                    }
                                    break;
                                }
                                Err(e) => {
                                    println!("Failed to request '{}', retrying in 1s: {}", url, e);
                                }
                            }
                            sleep(Duration::from_millis(1000)).await;
                        }
                        if !success {
                            match requeue_tx.send(task).await {
                                Ok(_) => {}
                                Err(e) => {
                                    println!("Failed to requeue result {}", e);
                                    return;
                                }
                            }
                            workers_failed.inc();
                            println!("Worker #{} is done. (has failed)", worker_i);
                            return;
                        } else {
                            referer = url;
                        }
                    }
                    Err(_) => {
                        // closed channel, done!
                        println!("Worker #{} is done.", worker_i);
                        return;
                    }
                }
            }
        });
    }
    drop(done_tx);
    let result_task = task::spawn(async move {
        match OpenOptions::new()
            .write(true)
            .append(true)
            .create(true)
            .open(out_file.as_str()).await {
            Ok(mut file) => {
                let mut file = BufWriter::new(&mut file);
                loop {
                    if let Some(found) = done_rx.recv().await {
                        match file.write_all(found.as_bytes()).await {
                            Ok(_) => {}
                            Err(e) => {
                                println!("Failed to write result to results file '{}': {}", out_file, e);
                                return;
                            }
                        }
                        if found.as_bytes()[found.len() - 1] != b'\n' {
                            match file.write_all(b"\n").await {
                                Ok(_) => {}
                                Err(e) => {
                                    println!("Failed to write result to results file '{}': {}", out_file, e);
                                    return;
                                }
                            }
                        }
                    } else {
                        match file.shutdown().await {
                            Ok(_) => {}
                            Err(e) => {
                                println!("Failed to close results file after being done '{}': {}", out_file, e);
                                return;
                            }
                        }
                        println!("Finished writing results");
                        return; // channel closed
                    }
                }
            }
            Err(e) => {
                println!("Failed to open results file '{}': {}", out_file, e);
            }
        }
    });

    while let Some(res) = tasks.join_next().await {
        match res {
            Ok(_) => {}
            Err(e) => {
                println!("Failed to join worker task: {}", e)
            }
        }
    }
    println!("Producer/Workers finished. Waiting for result writer now..");
    match result_task.await {
        Ok(_) => {}
        Err(e) => {
            println!("Failed to join result task: {}", e)
        }
    }
    let elapsed = start.elapsed();
    let worked = tasks_worked.get();
    let found = tasks_found.get();
    let failed = tasks_failed.get();
    println!("Worked {:07}, found {:07}, failed {:07}, ~{:.1}% exist, {:.1} req/s", worked, found, failed, found as f32 / worked as f32 * 100.0, worked as f32 / elapsed.as_secs_f32());
    if workers_failed.get() == worker_counter {
        println!("All workers failed :(");
        println!("This isn't a good sign, something is probably wrong, so we're blocking until manually killed");
        loop {
            sleep(Duration::from_secs(3600)).await;
        }
    }
    println!("All done.");
}