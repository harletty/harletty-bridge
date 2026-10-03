/*
 * Time a libavcodec decoder on an in-memory elementary stream.
 *
 * The counterpart of `harletty/examples/decode_bench.rs`, run against the
 * same inputs by `scripts/bench-vs-ffmpeg.sh`. Like it, this reads the whole
 * file into memory, feeds the codec's parser 64 KiB chunks and times parsing
 * plus decoding on one thread: no demuxer, no output, no ffmpeg CLI threads.
 *
 * Build (the script does this):
 *   cc -O2 -o ffmpeg_decode_bench ffmpeg_decode_bench.c \
 *       $(pkg-config --cflags --libs libavcodec libavutil)
 *
 * Usage:
 *   ffmpeg_decode_bench <decoder> <iterations> <input> [opt=value ...]
 *
 *   decoder   a libavcodec decoder name: truehd, ac3, eac3, dca
 *   opt=value decoder private options, e.g. drc_scale=0 or core_only=1
 *
 * Prints one JSON object on stdout.
 */

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include <libavcodec/avcodec.h>
#include <libavutil/avutil.h>
#include <libavutil/dict.h>
#include <libavutil/log.h>

#define CHUNK (64 * 1024)

typedef struct Tally {
    uint64_t frames;
    uint64_t samples; /* per channel, summed over frames */
    int channels;     /* widest frame */
    int sample_rate;
    uint64_t errors;
    const char *sample_fmt;
} Tally;

/* Written through so the compiler cannot discard the decoded samples. */
static volatile uint8_t sink;

static void drain_frames(AVCodecContext *ctx, AVFrame *frame, Tally *t)
{
    for (;;) {
        int ret = avcodec_receive_frame(ctx, frame);
        if (ret == AVERROR(EAGAIN) || ret == AVERROR_EOF)
            return;
        if (ret < 0) {
            t->errors++;
            return;
        }
        t->frames++;
        t->samples += frame->nb_samples;
        if (frame->ch_layout.nb_channels > t->channels)
            t->channels = frame->ch_layout.nb_channels;
        t->sample_rate = frame->sample_rate;
        t->sample_fmt = av_get_sample_fmt_name(frame->format);
        sink ^= frame->data[0][0];
        av_frame_unref(frame);
    }
}

static void send(AVCodecContext *ctx, AVPacket *pkt, AVFrame *frame, Tally *t)
{
    if (avcodec_send_packet(ctx, pkt) < 0)
        t->errors++;
    drain_frames(ctx, frame, t);
}

static Tally pass(const AVCodec *codec, const AVDictionary *opts,
                  const uint8_t *data, size_t size)
{
    Tally t = {0};
    AVCodecContext *ctx = avcodec_alloc_context3(codec);
    AVCodecParserContext *parser = av_parser_init(codec->id);
    AVPacket *pkt = av_packet_alloc();
    AVFrame *frame = av_frame_alloc();
    AVDictionary *o = NULL;
    av_dict_copy(&o, opts, 0);
    ctx->thread_count = 1;
    if (!ctx || !parser || !pkt || !frame || avcodec_open2(ctx, codec, &o) < 0) {
        fprintf(stderr, "cannot open decoder %s\n", codec->name);
        exit(70);
    }
    av_dict_free(&o);

    for (size_t off = 0; off < size; off += CHUNK) {
        const uint8_t *in = data + off;
        int in_size = (int)(size - off < CHUNK ? size - off : CHUNK);
        while (in_size > 0) {
            int used = av_parser_parse2(parser, ctx, &pkt->data, &pkt->size,
                                        in, in_size, AV_NOPTS_VALUE,
                                        AV_NOPTS_VALUE, 0);
            in += used;
            in_size -= used;
            if (pkt->size)
                send(ctx, pkt, frame, &t);
        }
    }
    /* Flush the parser's last packet, then the decoder. */
    av_parser_parse2(parser, ctx, &pkt->data, &pkt->size, NULL, 0,
                     AV_NOPTS_VALUE, AV_NOPTS_VALUE, 0);
    if (pkt->size)
        send(ctx, pkt, frame, &t);
    send(ctx, NULL, frame, &t);

    av_frame_free(&frame);
    av_packet_free(&pkt);
    av_parser_close(parser);
    avcodec_free_context(&ctx);
    return t;
}

static double now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1e3 + ts.tv_nsec / 1e6;
}

static int cmp_double(const void *a, const void *b)
{
    double x = *(const double *)a, y = *(const double *)b;
    return (x > y) - (x < y);
}

int main(int argc, char **argv)
{
    if (argc < 4) {
        fprintf(stderr, "usage: %s <decoder> <iterations> <input> [opt=value ...]\n", argv[0]);
        return 64;
    }
    const char *name = argv[1];
    int iterations = atoi(argv[2]);
    const char *input = argv[3];
    if (iterations <= 0) {
        fprintf(stderr, "iterations must be a positive integer\n");
        return 64;
    }

    const AVCodec *codec = avcodec_find_decoder_by_name(name);
    if (!codec) {
        fprintf(stderr, "no libavcodec decoder named %s\n", name);
        return 64;
    }
    AVDictionary *opts = NULL;
    for (int i = 4; i < argc; i++) {
        char *eq = strchr(argv[i], '=');
        if (!eq) {
            fprintf(stderr, "option %s is not opt=value\n", argv[i]);
            return 64;
        }
        *eq = '\0';
        av_dict_set(&opts, argv[i], eq + 1, 0);
    }
    av_log_set_level(AV_LOG_QUIET);

    FILE *f = fopen(input, "rb");
    if (!f) {
        perror(input);
        return 66;
    }
    fseek(f, 0, SEEK_END);
    size_t size = (size_t)ftell(f);
    fseek(f, 0, SEEK_SET);
    uint8_t *data = malloc(size);
    if (!data || fread(data, 1, size, f) != size) {
        fprintf(stderr, "cannot read %s\n", input);
        return 66;
    }
    fclose(f);

    /* One untimed pass, as on the harletty side. */
    Tally tally = pass(codec, opts, data, size);
    double *times = malloc(sizeof(double) * iterations);
    for (int i = 0; i < iterations; i++) {
        double start = now_ms();
        Tally t = pass(codec, opts, data, size);
        times[i] = now_ms() - start;
        if (t.frames != tally.frames || t.samples != tally.samples) {
            fprintf(stderr, "a pass decoded something else than the first\n");
            return 70;
        }
    }
    qsort(times, iterations, sizeof(double), cmp_double);

    printf("{\"decoder\":\"ffmpeg\",\"codec\":\"%s\",\"ffmpeg_version\":\"%s\","
           "\"input\":\"%s\",\"bytes\":%zu,\"frames\":%llu,\"samples\":%llu,"
           "\"channels\":%d,\"sample_rate\":%d,\"sample_fmt\":\"%s\","
           "\"errors\":%llu,\"audio_seconds\":%f,\"iterations\":%d,"
           "\"min_ms\":%f,\"median_ms\":%f}\n",
           name, av_version_info(), input, size,
           (unsigned long long)tally.frames, (unsigned long long)tally.samples,
           tally.channels, tally.sample_rate,
           tally.sample_fmt ? tally.sample_fmt : "none",
           (unsigned long long)tally.errors,
           tally.sample_rate ? (double)tally.samples / tally.sample_rate : 0.0,
           iterations, times[0], times[iterations / 2]);

    free(times);
    free(data);
    av_dict_free(&opts);
    return 0;
}
