# eac3

Independent E-AC-3 bitstream parser and frame extractor for Harletty Bridge.

The crate intentionally keeps a small surface:

- `extract::Extractor` incrementally extracts complete syncframes from a byte stream.
- `parser::parse_header` parses the syncframe header into allocation-free metadata.

It does not decode compressed audio to PCM.
