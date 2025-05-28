# Design

No-thread async: the workload is perfect for async and we don't need the overhead of threads.

# TODO

* Remove smol in favour of their respective crates (to speed up compilation?)
* update the stats faster than the logs?
* Remove unwraps where needed
* use io_uring for async file access?
