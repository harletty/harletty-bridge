// SPDX-License-Identifier: Apache-2.0
//
// Decode a raw .eac3 elementary stream with Cavern.Format and dump
// interleaved float32 PCM. Output channel order mirrors Cavern's
// ReferenceChannel enum ordering (which for a 5.1 stream gives FL FR FC LFE
// + the two surround channels in their enum-defined order), matching
// FFmpeg's 5.1(side) layout and the harletty-bridge `corpus_stream` example.
//
// EOF detection in Cavern is unreliable: `Cavern.Format.Utilities.StreamExtensions.ReadBytes`
// always returns a fully-allocated `byte[length]` (zero-padded on EOF), so
// `BlockBuffer.Readable` never flips to false, and after EOF the inner
// `BlockBuffer<float>` keeps returning stale zero-filled PCM forever. We
// therefore drive the decoder for a caller-provided frame budget. The
// expected frame count is the number of E-AC3 syncframes in the input,
// computed externally (typically by the harletty `corpus_stream` example)
// and passed via `--frames`. When omitted, we scan the input ourselves
// using the standard 0x0B77 syncword + 11-bit frmsiz header field.
//
// We also bypass `EnhancedAC3Reader.ReadHeader()` because its 2-arg
// `EnhancedAC3Decoder` ctor evaluates `Length = fileSize / frameSize *
// FrameSize` with `frameSize == 0` at construction time and throws
// `DivideByZeroException` for seekable streams. The 1-arg ctor leaves
// `Length = -1` and avoids that path.
//
// Usage:
//     dotnet run --project tools/cavern_pcm_dump -c Release -- \
//         <input.eac3> <output.f32> [--frames N]

using System;
using System.IO;
using System.Runtime.InteropServices;

using Cavern.Format.Decoders;
using Cavern.Format.Utilities;

namespace HarlettyBridge.CavernPcmDump;

internal static class Program {
    private static int Main(string[] args) {
        string? inputPath = null;
        string? outputPath = null;
        long? frameBudget = null;

        for (int i = 0; i < args.Length; i++) {
            string a = args[i];
            switch (a) {
                case "--frames":
                    if (i + 1 >= args.Length) { Usage(); return 64; }
                    frameBudget = long.Parse(args[++i]);
                    break;
                case "--help":
                case "-h":
                    Usage();
                    return 0;
                default:
                    if (inputPath == null) inputPath = a;
                    else if (outputPath == null) outputPath = a;
                    else { Usage(); return 64; }
                    break;
            }
        }

        if (inputPath == null || outputPath == null) { Usage(); return 64; }
        if (!File.Exists(inputPath)) {
            Console.Error.WriteLine($"input not found: {inputPath}");
            return 66;
        }

        long expectedFrames = frameBudget ?? CountSyncframes(inputPath);
        Console.Error.WriteLine($"[cavern] expected_frames={expectedFrames}");
        if (expectedFrames <= 0) {
            Console.Error.WriteLine($"[cavern] no syncframes detected in {inputPath}");
            return 70;
        }

        using FileStream input = File.OpenRead(inputPath);
        // Cavern's internal default (FormatConsts.blockSize) is 10 MB, but
        // it's `internal` so we can't reference it; pick the same value.
        const int BlockSize = 10 * 1024 * 1024;
        BlockBuffer<byte> buffer = BlockBuffer<byte>.CreateForConstantPacketSize(input, BlockSize);
        EnhancedAC3Decoder decoder = new(buffer);

        int channelCount = decoder.ChannelCount;
        int frameSize = decoder.FrameSize;
        int sampleRate = decoder.SampleRate;
        if (channelCount == 0 || frameSize == 0) {
            Console.Error.WriteLine($"[cavern] decoder reported channels={channelCount} frame_size={frameSize}; aborting");
            return 70;
        }
        Console.Error.WriteLine($"[cavern] channels={channelCount} sample_rate={sampleRate} samples_per_frame={frameSize}");

        using FileStream output = File.Create(outputPath);
        using BinaryWriter writer = new(output);

        long perFrameSamples = (long)channelCount * frameSize;
        float[] buf = new float[perFrameSamples];

        long frameCount = 0;
        int channelDriftFrame = -1;
        for (long i = 0; i < expectedFrames; i++) {
            try {
                decoder.DecodeBlock(buf, 0, perFrameSamples);
            } catch (NullReferenceException) {
                Console.Error.WriteLine($"[cavern] underlying stream reported EOF after frame {frameCount}");
                break;
            }
            byte[] bytes = MemoryMarshal.AsBytes(buf.AsSpan()).ToArray();
            writer.Write(bytes);
            frameCount++;

            if (decoder.ChannelCount != channelCount && channelDriftFrame < 0) {
                channelDriftFrame = (int)frameCount;
            }
        }

        long bytesWritten = output.Position;
        long samplesPerCh = bytesWritten / 4 / channelCount;
        Console.Error.WriteLine($"[cavern] frames={frameCount} samples_per_channel={samplesPerCh} bytes={bytesWritten}");
        if (channelDriftFrame >= 0) {
            Console.Error.WriteLine($"[cavern] WARNING channel count drift first observed at frame {channelDriftFrame}");
        }
        if (frameCount < expectedFrames) {
            Console.Error.WriteLine($"[cavern] WARNING decoded {frameCount} frames, expected {expectedFrames}");
        }
        return 0;
    }

    private static void Usage() {
        Console.Error.WriteLine("usage: cavern_pcm_dump <input.eac3> <output.f32> [--frames N]");
    }

    // Scan the file for E-AC3 syncframes (0x0B77 + 11-bit frmsiz at bits
    // 16-26). Returns the number of valid frames found. Skips invalid bytes
    // 1 at a time so partial corruption doesn't desync the count.
    private static long CountSyncframes(string path) {
        using FileStream fs = File.OpenRead(path);
        long count = 0;
        byte[] hdr = new byte[5];
        int read;
        long pos = 0;
        long len = fs.Length;
        while (pos + 5 <= len) {
            fs.Position = pos;
            read = fs.Read(hdr, 0, 5);
            if (read < 5) break;
            if (hdr[0] == 0x0B && hdr[1] == 0x77) {
                int frmsiz = ((hdr[2] & 0x07) << 8) | hdr[3];
                int frameSize = (frmsiz + 1) * 2;
                if (frameSize >= 6 && pos + frameSize <= len) {
                    count++;
                    pos += frameSize;
                    continue;
                }
            }
            pos++;
        }
        return count;
    }
}
