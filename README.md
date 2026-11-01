# speedtest

A terminal speed test: download, upload and ping, with a live bar for each throughput phase.

```
Server    Dallas, US
Download  ▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▱▱▱▱▱▱▱▱▱    412.5 Mbps  (0–500)
Upload    ▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱     47.3 Mbps  (0–100)
Ping      min 12.3 ms   mean 14.1 ms   max 19.8 ms   (10 samples)
```

In a terminal, every bar cell is the same `▰` glyph, white for the lit part and gray for the rest.
The bar redraws in place while the phase runs and stays at the overall average when it ends.
The scale starts at 0–10 Mbps and grows to the next of 20, 50, 100, 200, 500, 1000 and so on as the speed exceeds it.
When stdout is not a terminal, only the finished lines are printed.

## Install

```
cargo install --path . --locked
```

or `make install-cli`.

## Usage

```
speedtest                  # nearest M-Lab server, 10 s per phase, 10 pings
speedtest --cloudflare     # Cloudflare's speed test endpoints instead
speedtest --duration 5     # shorter download and upload phases
```

| Flag              | Default | Meaning                                                                          |
| ----------------- | ------- | -------------------------------------------------------------------------------- |
| `--mlab`          | on      | Measure against the nearest [M-Lab](https://www.measurementlab.net/) NDT7 server |
| `--cloudflare`    | off     | Measure against `speed.cloudflare.com` instead                                   |
| `--duration SECS` | 10      | Length of each throughput phase; M-Lab servers end the test at 10 s              |
| `--connections N` | 4       | Parallel streams for Cloudflare; NDT7 is one stream by design                    |
| `--pings N`       | 10      | Latency samples                                                                  |

## How it measures

**M-Lab** runs the servers behind the speed test in Google Search and Google Fiber.
The CLI asks `locate.measurementlab.net` for the nearest server, which is the location on the `Server` line, then runs
the [NDT7 protocol](https://github.com/m-lab/ndt-server/blob/main/spec/ndt7-protocol.md) over WebSockets.
Download counts the bytes the server streams for up to ten seconds.
Upload sends binary frames, starting at 8 KiB and doubling up to 16 MiB as the spec describes, until the deadline or the
server closes the socket.

**Cloudflare** uses the same endpoints as the test at `speed.cloudflare.com`: GET `__down?bytes=N` and POST `__up`.
Each of the parallel connections requests 25 MB at a time until the deadline.
Cloudflare returns 403 for larger requests and 429 after roughly an hour of heavy use from one address, with a
`Retry-After` header saying when the limit clears.
That is why M-Lab is the default.

**Ping** is an HTTP round trip to the chosen server, repeated on one warm connection after a discarded warm-up request.
It is not ICMP, so it needs no privileges.
If the first five samples take more than four seconds in total, sampling stops at five.

The live bar shows throughput over the last second.
The final figure is total bytes over the whole phase.
For upload the count is bytes accepted by the socket, which can run slightly ahead of what has reached the server on
slow links.
At the deadline the upload waits for the server to finish the exchange before the average is taken.

## Development

```
make format     # rustfmt
make check      # clippy with the pedantic group as errors
make test       # unit tests
make install-dev  # build and install pre-commit hooks
```
