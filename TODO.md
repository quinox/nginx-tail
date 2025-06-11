# TODO

* Remove smol in favour of their respective crates? To speed up compilation
* make frequency of lines/stats printing configurable
* Remove unwraps where possible
* use io_uring for async file access?
* A 'total' line when there are lots of group lines
* Support reading from stdin
* Split `--filter` into `--include` and `--exclude`
* Mark files that have an old mtime as grey
* Update stats frequency automatically for low-volume servers?
* Handle stats being wider than the screen?
* Combine 301/302/307/308 etc. by default
* print lines-per-second as well (since sampled is often at 0%)
