#!/usr/bin/env python3
"""Expose only selected codecs from a pinned, public Delta Web debug module."""

import argparse
import hashlib
import json
import mmap
from pathlib import Path
import re


BUILD = "45918daa2b408d0f2efa573d44165d038c0d3109"
SHA256 = "e10998d63abd1a7ea7fafe21d8216ee2612a8d749e8c46d0701eb7b969fae13d"
ASSET = "delta_web-8204d5b93885d614_bg.debug.wasm"
URL = f"https://cdn.delta.dev/delta-web/production/{BUILD}/symbols/{ASSET}"
HEADER = b"\0asm\1\0\0\0"
ENCODE = "encode_versioned::<alloc::vec::Vec<u8>>"
DECODE = "decode_versioned::<&[u8]>"


def codec(type_name, method):
    return f"<delta_remote_protocol::{type_name} as versioned::Versioned>::{method}"


FUNCTIONS = {
    "init_memory": "__wasm_init_memory",
    "malloc": "malloc",
    "git_header_encode": codec("GitBlobFrameHeader", ENCODE),
    "git_header_decode": codec("GitBlobFrameHeader", DECODE),
    "start_turn_encode": codec("StartTurn", ENCODE),
    "set_local_encode": codec("SetLocalMachineRunner", ENCODE),
    "ack_local_encode": codec("AcknowledgeLocalMachineTurn", ENCODE),
    "client_encode": codec("ClientMessage", ENCODE),
    "server_decode": codec("ServerMessage", DECODE),
    "schema_thread": "<thread::ThreadDataStub as versioned::Versioned>::versioned_schema",
    "schema_batch": (
        "<versioned::schema::SchemaCollector>::register::<"
        "deltadb::history::HistoryDeltaBatch, "
        "<deltadb::history::HistoryDeltaBatch as versioned::Versioned>"
        "::versioned_schema::{closure#0}>"
    ),
}


class Reader:
    def __init__(self, data, start=0, end=None):
        self.data = data
        self.pos = start
        self.end = len(data) if end is None else end

    def take(self, size):
        if size < 0 or self.pos + size > self.end:
            raise ValueError("truncated Wasm section")
        result = self.data[self.pos:self.pos + size]
        self.pos += size
        return result

    def uint(self):
        result = 0
        for shift in range(0, 35, 7):
            byte = self.take(1)[0]
            result |= (byte & 127) << shift
            if not byte & 128:
                if result > 0xFFFFFFFF:
                    raise ValueError("Wasm u32 overflow")
                return result
        raise ValueError("invalid Wasm u32")

    def text(self):
        return self.take(self.uint()).decode("utf-8")


def uint(value):
    if not 0 <= value <= 0xFFFFFFFF:
        raise ValueError("Wasm u32 out of range")
    result = bytearray()
    while True:
        byte = value & 127
        value >>= 7
        result.append(byte | (128 if value else 0))
        if not value:
            return bytes(result)


def text(value):
    value = value.encode("utf-8")
    return uint(len(value)) + value


def sections(data):
    reader = Reader(data)
    if reader.take(8) != HEADER:
        raise ValueError("not a Wasm 1 module")
    while reader.pos < reader.end:
        kind = reader.take(1)[0]
        length = reader.uint()
        start = reader.pos
        reader.take(length)
        yield kind, start, start + length


def imports(data, start, end):
    reader = Reader(data, start, end)
    result = []
    for _ in range(reader.uint()):
        entry = {"module": reader.text(), "name": reader.text()}
        kind = reader.take(1)[0]
        if kind == 0:
            reader.uint()  # Function type; supplied as a throwing host stub.
        elif kind == 2:
            flags = reader.uint()
            if flags != 3:
                raise ValueError("expected bounded, shared 32-bit memory")
            entry["memory"] = {
                "initial": reader.uint(), "maximum": reader.uint(), "shared": True,
            }
        else:
            raise ValueError(f"unexpected import kind {kind}")
        result.append(entry)
    if reader.pos != end:
        raise ValueError("trailing import section data")
    return result


def selected_names(data, start, end):
    reader = Reader(data, start, end)
    found = {}
    wanted = {name: export for export, name in FUNCTIONS.items()}
    while reader.pos < end:
        kind = reader.take(1)[0]
        length = reader.uint()
        stop = reader.pos + length
        if stop > end:
            raise ValueError("truncated name subsection")
        if kind == 1:
            names = Reader(data, reader.pos, stop)
            for _ in range(names.uint()):
                index, name = names.uint(), names.text()
                normalized = re.sub(r"\[[0-9a-f]+\]", "", name)
                if normalized in wanted:
                    export = wanted[normalized]
                    if export in found:
                        raise ValueError(f"ambiguous function {normalized}")
                    found[export] = index
        reader.take(length)
    return found


def prepare(data, output):
    if hashlib.sha256(data).hexdigest() != SHA256:
        raise ValueError("debug module does not match the pinned SHA-256")
    parts = list(sections(data))
    if any(kind == 8 for kind, _, _ in parts):
        raise ValueError("implicit Wasm start function forbidden")
    selected, host_imports = {}, None
    for kind, start, end in parts:
        if kind == 2:
            host_imports = imports(data, start, end)
        if kind == 0:
            reader = Reader(data, start, end)
            if reader.text() == "name":
                selected = selected_names(data, reader.pos, end)
    if selected.keys() != FUNCTIONS.keys() or host_imports is None:
        raise ValueError("reference module is missing required functions/imports")
    # Do not retain any original exports: no main, UI, auth, or thread startup.
    exports = [("memory", 2, 0)]
    exports.extend((name, 0, selected[name]) for name in FUNCTIONS)
    export_data = uint(len(exports)) + b"".join(
        text(name) + bytes([kind]) + uint(index) for name, kind, index in exports
    )
    output.mkdir(parents=True, exist_ok=True)
    with (output / "reference.wasm").open("wb") as target:
        target.write(HEADER)
        for kind, start, end in parts:
            if kind == 0:
                continue  # Strip names and DWARF, not executable code or data.
            payload = export_data if kind == 7 else data[start:end]
            target.write(bytes([kind]) + uint(len(payload)) + payload)
    metadata = {
        "version": "0.24.2", "build": BUILD, "source_sha256": SHA256,
        "source_url": URL, "imports": host_imports, "functions": selected,
    }
    (output / "reference.json").write_text(json.dumps(metadata, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--debug-wasm", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    with args.debug_wasm.open("rb") as source:
        with mmap.mmap(source.fileno(), 0, access=mmap.ACCESS_READ) as data:
            prepare(data, args.output_dir)
    print(f"Prepared codec-only exports in {args.output_dir}")


if __name__ == "__main__":
    main()
