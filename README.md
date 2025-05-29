# Design

No-thread async: the workload is perfect for async and we don't need the overhead of threads. Blocking IO will be done in separate threads until we can master io_uring

Minimal set of dependencies. If we can easily build something ourselves we should. This is educational and reduces supply chain risks. The software only needs to run on the two Ubuntu LTS versions so we don't need to overly target cross compatibility.



# TODO

* Remove smol in favour of their respective crates (to speed up compilation?)
* support updating the stats faster than the logs
* colored logs
* Remove unwraps where needed
* use io_uring for async file access?
* Total line when there are lots of group lines
* Turn the cursor off
* Support reading from stdin
