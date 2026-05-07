# eac3

Independent E-AC-3 bitstream parser and decoder for Harletty Bridge.

The crate intentionally keeps a small surface:

- `extract::Extractor` incrementally extracts complete syncframes from a byte stream.
- `parser::parse_header` parses the syncframe header into allocation-free metadata.
- `inspect_access_unit` parses complete access units, including EMDF/OAMD/JOC metadata.
- `PcmDecoder` decodes core channel PCM.
- `ObjectPcmDecoder` decodes core PCM plus dynamic object channels when JOC payloads are present.

Transport handling and conversion to the bridge ABI remain outside this crate.
