# nginx-tail, better than `tail -f`

## About this project

A tool to keep an eye on `nginx` access logs, especially on busy servers (1000+
requests per second).

This is a personal project. I'm not likely to accept PRs; I do welcome bug
report, ideas and feature requests.

## Screenshots

todo

## Design

It should do "the right" thing without configuration. A user should have a good
time using this tool without having to dig into the command line options. This
often means the tool will toggle options on/off by itself based on the
circumstances, with the option to force a state through command line flags.

Threadless async: the workload is perfect for async and we don't need the
overhead of threads. Blocking IO will be done in separate threads until we can
master io_uring.

Minimal set of dependencies. If we can easily build something ourselves we
should. This is educational and reduces supply chain risks. The software only
needs to run on a specific set of servers so we don't need to overly target
cross compatibility. Releases are built with `musl` to improve portability but
that's it.

## TODO

* Remove smol in favour of their respective crates? To speed up compilation
* support updating the stats faster than the logs
* Remove unwraps where possible
* use io_uring for async file access?
* A 'total' line when there are lots of group lines
* Support reading from stdin
* CI
