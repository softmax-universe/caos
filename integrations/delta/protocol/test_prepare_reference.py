import hashlib
import unittest
from unittest.mock import patch

from prepare_reference import (
    FUNCTIONS, HEADER, Reader, imports, prepare, sections, selected_names, text, uint,
)


class ReaderTests(unittest.TestCase):
    def test_unsigned_integer_boundaries(self):
        for value in [0, 127, 128, 65535, 65536, 0xFFFFFFFF]:
            with self.subTest(value=value):
                encoded = uint(value)
                reader = Reader(encoded)
                self.assertEqual(reader.uint(), value)
                self.assertEqual(reader.pos, len(encoded))

    def test_truncated_and_overflowing_integers(self):
        for encoded in [b"", b"\x80", b"\xff\xff\xff\xff\x1f",
                        b"\x80\x80\x80\x80\x80\x00"]:
            with self.subTest(encoded=encoded):
                with self.assertRaises(ValueError):
                    Reader(encoded).uint()

    def test_out_of_range_integer_encoding_rejected(self):
        for value in [-1, 0x100000000]:
            with self.subTest(value=value):
                with self.assertRaises(ValueError):
                    uint(value)

    def test_utf8_name(self):
        self.assertEqual(Reader(text("CAOS – eshear")).text(), "CAOS – eshear")

    def test_section_bounds(self):
        module = HEADER + b"\0" + uint(3) + b"abc"
        self.assertEqual(list(sections(module)), [(0, 10, 13)])
        for invalid in [b"", module[:-1]]:
            with self.assertRaises(ValueError):
                list(sections(invalid))

    def test_function_and_shared_memory_imports(self):
        data = (
            uint(2) + text("host") + text("fetch") + b"\0" + uint(5)
            + text("host") + text("memory") + b"\x02\x03"
            + uint(194) + uint(32768)
        )
        self.assertEqual(imports(data, 0, len(data)), [
            {"module": "host", "name": "fetch"},
            {"module": "host", "name": "memory",
             "memory": {"initial": 194, "maximum": 32768, "shared": True}},
        ])

    def test_unexpected_import_kind_rejected(self):
        data = uint(1) + text("host") + text("table") + b"\x01"
        with self.assertRaises(ValueError):
            imports(data, 0, len(data))

    def test_select_names_without_exporting_application(self):
        names = uint(2) + uint(10) + text("main") + uint(20) + text(
            FUNCTIONS["git_header_encode"].replace(
                "delta_remote_protocol::", "delta_remote_protocol[123abc]::",
            ),
        )
        data = b"\x01" + uint(len(names)) + names
        self.assertEqual(selected_names(data, 0, len(data)),
                         {"git_header_encode": 20})

    def test_ambiguous_function_rejected(self):
        names = uint(2) + uint(1) + text("malloc") + uint(2) + text("malloc")
        data = b"\x01" + uint(len(names)) + names
        with self.assertRaises(ValueError):
            selected_names(data, 0, len(data))

    def test_truncated_name_subsection_rejected(self):
        data = b"\x01\x05\x00"
        with self.assertRaises(ValueError):
            selected_names(data, 0, len(data))

    def test_unknown_source_hash_refused_before_writing(self):
        with self.assertRaisesRegex(ValueError, "pinned SHA-256"):
            prepare(HEADER, None)

    def test_implicit_application_start_refused_before_writing(self):
        data = HEADER + b"\x08\x01\x00"
        with patch("prepare_reference.SHA256", hashlib.sha256(data).hexdigest()):
            with self.assertRaisesRegex(ValueError, "start function forbidden"):
                prepare(data, None)


if __name__ == "__main__":
    unittest.main()
