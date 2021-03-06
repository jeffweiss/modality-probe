# modality-probe-graph

This crate provides types and implementations for building a graph out
of a probe's report.

## Usage

Include it in your `Cargo.toml`:

```toml
[dependencies]
modality-probe-graph = { git = git@github.com:auxoncorp/modality-probe }
```

Example usage:

```rust
pub(super) fn log_to_graph<I>(
    log: Peekable<I>,
) -> Result<EventDigraph<NodeAndEdgeLists<GraphEvent>>, VisualizeError>
where
    I: Iterator<Item = ReportLogEntry>,
{
    let mut graph = EventDigraph::new(NodeAndEdgeLists {
        nodes: HashSet::new(),
        edges: HashSet::new(),
    });
    let report_iter = ReportIter::new(log);
    for report in report_iter {
        graph.add_report(&report).map_err(|e| {
            VisualizeError(format!(
                "encountered an error reconstructing the graph: {}",
                e
            ))
        })?;
    }
    Ok(graph)
}
```

## Running the tests

Use Cargo:

```shell
$ cargo test
```

## License

See [LICENSE](../LICENSE) for more details.

Copyright 2020 Auxon Corporation

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

[http://www.apache.org/licenses/LICENSE-2.0](http://www.apache.org/licenses/LICENSE-2.0)

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
