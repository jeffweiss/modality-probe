# ekotrace

A distributed causal tracing system suitable for embedded use.

> **This work is currently in progress. We are planning to make
> changes and will note [here](CHANGELOG.md) when you will need
> to make an update.**

## Background

`ekotrace` tracks events of interest in a system as well as
the causal relationships of those events.

The trace data that `ekotrace` outputs shows, for example,
whether event B happened because of event A, or
whether event B happened independently of event A.

Causal tracing works without relying on system clocks at all.

`ekotrace` can trace one or many embedded components and provides
information about the impact of each component on the system as
a whole.

## Overview

The core of `ekotrace` is the [reference client library implementation](ekotrace),
which provides the capabilities developers need to start capturing trace
data in their systems of interest.

```rust
// Initialization of an ekotrace instance only needs to happen once
// in the lifetime of a thread / execution context
let ekotrace_foo = Ekotrace::initialize_at(&mut storage_bytes, LOCATION_ID_FOO)?;
ekotrace_foo.record_event(EVENT_A);
ekotrace_foo.record_event(EVENT_B);
```

The above demonstrates using an `ekotrace` instance to record some events. These events are
in that instance's timeline.
`ekotrace` instances identify specific points in time by making causal snapshots.

```rust
let mut snapshot_buffer = [0u8; 256];
let n_snapshot_bytes = ekotrace_foo.distribute_snapshot(&mut snapshot_buffer)?;
```

To connect the dots between the timelines of `ekotrace` instances in different locations,
distribute and merge those causal snapshots.

```rust
// ... in a different component in the system ...

// ekotrace_bar, the local ekotrace instance, would have been created
// at the start of this process / thread
let ekotrace_bar = Ekotrace::initialize_at(&mut storage_bytes, LOCATION_ID_BAR)?;

// Assume the user has made the bytes written by `ekotrace_foo` into `snapshot_buffer` available
// (e.g. through messaging)
ekotrace_bar.merge_snapshot(&snapshot_buffer[..n_snapshot_bytes])?;
// From this point on, events recorded by ekotrace_bar can be sure
// to have happened after / because of the events at ekotrace_foo at the time
// of that snapshot.
ekotrace_bar.record_event(EVENT_C);
```

The `ekotrace` client provides a system-agnostic approach to reporting. `ekotrace` instances
write their report data into a byte buffer, which can be sent to a report collecting backend
whichever way the system under test prefers. Report content can be treated as opaque
bytes by the embedded system.

```rust
let mut report_buffer = [0u8; 1024];
let n_report_bytes = ekotrace.report(&mut report_buffer)?;
send_bytes(&report_buffer[..n_report_bytes]); // send_bytes is user-defined
```

The [ekotrace-udp-collector](ekotrace-udp-collector) is a reference implementation of a
trace data logging backend. It uses UDP to receive `ekotrace` report bytes from the client
(bytes which conform to a [standardized schema](schemas/log_reporting.lcm)) and logs
trace data to a file for offline analysis.

## Id Management

`ekotrace` requires the user to provide a unique id for each distinct location (read: execution context) of interest
in the system. Practically speaking, this means one tracer-location id per thread. In order to
help coordinate id management, an optional [id definition file format](ekotrace-header-gen/README.md#id-management-format) is defined.

`ekotrace` also requires the user to provide unique ids for the events they care about, which may
be managed in a similar fashion as tracer ids.

Event ids can be automatically generated for a project that uses the `ekotrace` API
with the [ekotrace-event-manifest-gen](ekotrace-event-manifest-gen) tool.

The [ekotrace-header-gen](ekotrace-header-gen) tool automates away id-in-code definition effort.

## APIs

The client library has a [C API](ekotrace-capi) and a `no_std` no-alloc compatible [Rust API](ekotrace) available.

See the [C API README](ekotrace-capi/README.md) for directions on using
`ekotrace` from C as a static library.

## Automated Workflow Example

Define your tracers, in `tracers.csv` for this example.

```csv
id,name,description
1,location_id_foo,Location Foo
```

Start using the `ekotrace` API, don't worry about defining your event identifiers.

```rust
// src/tracing_ids.rs mod generated by ekotrace-header-gen
use crate::tracing_ids::*;

// Initialization of an ekotrace instance only needs to happen once
// in the lifetime of a thread / execution context
let ekotrace_foo = Ekotrace::try_initialize_at(&mut storage_bytes, LOCATION_ID_FOO)?;

// Record events
if match bar() {
    true => ekotrace_foo.record_event(EVENT_A),
    false => ekotrace_foo.record_event(EVENT_B),
}
```

Add a build script to generate the event ids and definitions.

```toml
# Cargo.toml
build = "build.rs"
```

```rust
# build.rs
use std::fs::File;
use std::io::{self, Write};
use std::process::Command;

fn main() {
    // Generate `events.csv` event id manifest file, searching in src/ directory
    let status = Command::new("ekotrace-event-manifest-gen")
        .arg("--events-csv-file")
        .arg("events.csv")
        .arg("src/")
        .status()
        .expect("ekotrace-event-manifest-gen failed unexpectedly");
    assert!(status.success());

    // Generate Rust definitions for event and tracer ids in `src/tracing_ids.rs`
    let mut events_src = File::create("src/tracing_ids.rs").expect("Could not make file");
    let output = Command::new("ekotrace-header-gen")
        .arg("--lang")
        .arg("Rust")
        .arg("events.csv")
        .arg("tracers.csv")
        .output()
        .expect("ekotrace-header-gen failed unexpectedly");
    io::stderr().write_all(&output.stderr).expect("Could not write to stderr");
    events_src.write_all(&output.stdout).expect("Could not write to stdout");
    assert!(output.status.success());
}
```

Building the project now generates `events.csv` and `src/tracing_ids.rs`

```bash
cargo build
```

Users can further annotate the event and tracer id files with descriptions
and additional definitions.

```csv
id,name,description
1,event_a,Event A occurred
2,event_b,Event B was triggered
3,event_c,Event C for future use
3,event_d,Event D also for future use
```