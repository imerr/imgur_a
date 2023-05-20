imgur_a
====
Fast tool to scan for valid 5-long imgur albums for the [ArchiveTeam imgur efforts](https://wiki.archiveteam.org/index.php/Imgur) (not affiliated or endorsed)

Uses supplied http proxies to scan many ids in parallel since imgur does have rate limiting.

Takes a list of ids to work off of, so please coordinate in [#imgone on hackint](https://webirc.hackint.org/#irc://irc.hackint.org/imgone)

# Usage
```
Usage: imgur_a [OPTIONS] -i <INPUT_FILE> -r <RESULTS_FILE>

Options:
  -i, --input-file <INPUT_FILE>      Where to read ids from
  -r, --results-file <RESULTS_FILE>  Where to save found results
  -p, --proxy-file <PROXY_FILE>      This specifies an optional list of http proxies to use
                                     Proxy list file has the format of 'PROXY_HOST:PROXY_PORT:PROXY_USER:PROXY_PASSWORD' with one entry per line
                                     So for example 'proxy.example.com:1234:username:password123'
                                     For each entry, one worker will be spawned.
  -c, --concurrent <CONCURRENT>      How many requests to queue per second (actual rate will be slightly lower) [default: 3]
      --concurrent-unsafe            Bypass concurrency sanity check
  -h, --help                         Print help
  -V, --version                      Print version
```

# Building
Github Actions are set up to provide builds, but especially the linux ones might not run on your distro

Building is easy though!

1. [Install rust](https://www.rust-lang.org/tools/install)
2. Install your platforms compiler toolchain (for debian-based distros this would be `apt install build-essential`, for windows this might be MSVC)
3. Clone this repo or [download it as a .zip](https://github.com/imerr/imgur_id7/archive/refs/heads/main.zip)
4. Run `cargo build --release`* and grab the resulting binary from `target/release/imgur_id7`
5. Success!

*You might have to install library headers like `libssl-dev` and `pkg-config`, but the build process will complain accordingly 