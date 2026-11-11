module github.com/softmax-universe/caos/integrations/delta/connector

go 1.25.0

require (
	caos v0.0.0
	github.com/coder/websocket v1.8.15
	github.com/vmihailenco/msgpack/v5 v5.4.1
)

require (
	github.com/bitfield/script v0.25.0 // indirect
	github.com/itchyny/gojq v0.12.13 // indirect
	github.com/itchyny/timefmt-go v0.1.5 // indirect
	github.com/vmihailenco/tagparser/v2 v2.0.0 // indirect
	mvdan.cc/sh/v3 v3.7.0 // indirect
)

replace caos => ../../../std/go/prelude
