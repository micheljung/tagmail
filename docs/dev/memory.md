# Memory

Idle RSS of `tagmail-server` after syncing the 100k seed (`cargo xtask
bench-rss`, 300 s idle, glibc build in `debian:trixie-slim`, Dovecot network).
Three runs were started in parallel per round; the gated figure is the
highest RSS of the last 60 s.

| Build | Round 1 | Round 2 |
|-------|---------|---------|
| glibc malloc, default tokio workers | 23.6 MiB | 55.2 MiB |
| glibc malloc, 2 workers | 42.7 MiB | 43.1 MiB |
| jemalloc (`narenas:2`, 1 s decay), 2 workers | 25.7 MiB | 31.5 MiB |

glibc's idle RSS is bimodal (it trims after the text pass or it never does),
so jemalloc, which stays at 26 to 32 MiB, is the worst-case winner. Sync peak
(~54 MiB) and VmHWM (~52 MiB) are the same with all three.

On musl static (`--in-dovecot-network`) jemalloc measured worse than musl's
own allocator (idle 16.5 vs 11.2 MiB, peak 49.7 vs 19.0, VmHWM 48.9 vs 27.0),
so jemalloc is only linked on non-musl targets. Its settings
(`JEMALLOC_SYS_WITH_MALLOC_CONF` in `.cargo/config.toml`) are compiled in
because `malloc_conf` cannot be exported from safe Rust.

`--worker-threads` (default 2) and `--max-blocking-threads` (default 8) cap
the tokio pools.
