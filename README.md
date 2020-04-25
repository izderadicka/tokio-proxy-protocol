tokio-proxy-protocol
====================

[proxy protocol](http://www.haproxy.org/download/1.8/doc/proxy-protocol.txt) for tokio

Only v1 of protocol is implemented.

**WARNING**: Alpha quality code

How to test with HAProxy
------------------------

Add this to HAProxy config:

```
listen  my-test
	bind 127.0.0.1:7777
	mode tcp
	server server1 127.0.0.1:7776 send-proxy
```

and restart haproxy

In `examples` there are two programs `backend` and `proxy`. `proxy` is an example of another proxy behind HAProxy, it reads PROXY header, logs information about original addresses and then passes this info to downstream connection to `backend`, which can then see the original address passed by HAProxy - so setup is:

```
[client] <--> [HAProxy] <--> [proxy] <--> [backend]
```
Run them both in separate terminals

```
RUST_LOG=debug cargo run --example backend
```

```
RUST_LOG=debug cargo run --example proxy
```

send data to server with `netcat`

```
cat some_file | nc -N localhost 7777
```
