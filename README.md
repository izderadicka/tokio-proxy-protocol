tokio-proxy-protocol
====================

POC for [proxy protocol](http://www.haproxy.org/download/1.8/doc/proxy-protocol.txt) for tokio

Experimental only - do not use!

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

run 
```
RUST_LOG=debug cargo run --example server
```

send data to server
```
cat some_file | nc -N localhost 7777
```
