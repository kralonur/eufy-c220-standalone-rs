#define _GNU_SOURCE
#include <aacdecoder_lib.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/videodev2.h>
#include <netinet/in.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

/*
 * C220 unified camera-side media engine.
 *
 * Owns the Rockchip RV1103/RV1106 multimedia hardware once, in one process:
 *   - AIQ + V4L2 + MPI VENC        -> H.265 / HEVC video (Main profile)
 *   - MPI AI + AENC                -> G.711 A-law mono microphone
 *   - MPI AO + FDK AAC             -> speaker / talkback playback
 *
 * All media and commands travel over one AF_UNIX stream socket using the
 * 20-byte "C220" header. This replaces the three separate helper processes.
 *
 * Header layout (little-endian):
 *   0..3   "C220"
 *   4..7   payload length
 *   8..15  timestamp in microseconds
 *   16     video keyframe flag
 *   17     record kind: 0 video, 1 audio capture,
 *                      2 speaker PCM, 3 speaker clear, 4 AO volume
 *   18     video codec (1 = H.264, 2 = H.265); 0 otherwise
 *   19     reserved
 */

uintptr_t __stack_chk_guard = 0x595e9fbd;

#define RK_SUCCESS 0
#define RK_FALSE 0
#define RK_TRUE 1
typedef int32_t RK_S32;
typedef uint32_t RK_U32;
typedef int32_t RK_BOOL;
typedef void *MB_BLK;

#define RK_VIDEO_ID_HEVC 12
#define RK_FMT_YUV420SP 0
#define H265E_PROFILE_MAIN 1
#define VENC_RC_MODE_H265CBR 8
#define VENC_GOPMODE_NORMALP 1

#define AUDIO_DEV int32_t
#define AI_CHN int32_t
#define AO_CHN int32_t
#define AENC_CHN int32_t
#define RK_AUDIO_ID_PCM_ALAW ((int32_t)0x40000002)
#define AUDIO_SAMPLE_RATE_8000 8000
#define AUDIO_SAMPLE_RATE_16000 16000
#define AUDIO_BIT_WIDTH_16 1
#define AUDIO_SOUND_MODE_MONO 0
#define AUDIO_SOUND_MODE_STEREO 1
#define AUDIO_TRACK_FRONT_LEFT 8
#define AUDIO_TRACK_OUT_STEREO 10
#define AUDIO_CHN_MODE_DEFAULT 0
#define AUDIO_LOOPBACK_NONE 0
#define RK_ID_AI 10
#define RK_ID_AENC 12
#define MAX_AUDIO_CARD_NAME 64
#define AUDIO_FRAME_SAMPLES 320
#define AUDIO_MAX_PAYLOAD 4096
#define MAX_AO_VOLUME 100
#define DEFAULT_AO_VOLUME 100
#define PCM_ATTENUATION_SHIFT 0
#define SPEAKER_GPIO 102
#define MAX_PCM_PAYLOAD (64U * 1024U)
#define MAX_AAC_PAYLOAD (48U * 1024U)

#define WIDTH 2304U
#define HEIGHT 1296U
/* Detection sub-stream. 640x360 keeps the 16:9 aspect of the 2K main stream, so
 * detectors letterbox it instead of seeing a squashed image. The scale factor
 * is exactly 3.6:1 on both axes, which is why the downscale below can use one
 * fixed-point phase for rows and columns alike. */
#define SUB_WIDTH 640U
#define SUB_HEIGHT 360U
#define SUB_FRAME_SIZE ((size_t)SUB_WIDTH * SUB_HEIGHT * 3U / 2U)
#define SUB_DEFAULT_FPS 5
#define SUB_DEFAULT_BITRATE_KBPS 384
#define SUB_STREAM_BUF_SIZE (256U * 1024U)
#define MAX_SUB_AU_SIZE (128U * 1024U)
#define VENC_SUB_CHANNEL 2
#define FRAME_SIZE ((size_t)WIDTH * HEIGHT * 3U / 2U)
#define MAX_AU_SIZE (1024U * 1024U)
#define NUM_INPUT_BUFFERS 1
#define FRAME_HEADER_LEN 20

/* Wire codec byte carried in the record header for video records. */
#define CODEC_H265 2

#define KIND_VIDEO 0
#define KIND_AUDIO_CAPTURE 1
#define KIND_VIDEO_SUB 5
#define KIND_SPEAKER_PCM 2
#define KIND_SPEAKER_CLEAR 3
#define KIND_SPEAKER_VOLUME 4
#define KIND_NIGHT_MODE 6

/* --- Rockchip VENC ABI --- */
typedef struct {
    uint32_t u32Gop;
    uint32_t u32SrcFrameRateNum;
    uint32_t u32SrcFrameRateDen;
    uint32_t fr32DstFrameRateNum;
    uint32_t fr32DstFrameRateDen;
    uint32_t u32BitRate;
    uint32_t u32StatTime;
    uint32_t u32Resv[2];
} VENC_CBR_S;

typedef struct {
    int32_t enRcMode;
    VENC_CBR_S stH265Cbr;
} VENC_RC_ATTR_S;

typedef struct {
    int32_t enGopMode;
    uint32_t u32Resv[3];
} VENC_GOP_ATTR_S;

typedef struct {
    int32_t enType;
    uint32_t u32MaxPicWidth;
    uint32_t u32MaxPicHeight;
    int32_t enPixelFormat;
    int32_t enMirror;
    uint32_t u32BufSize;
    uint32_t u32Profile;
    int32_t bByFrame;
    uint32_t u32PicWidth;
    uint32_t u32PicHeight;
    uint32_t u32VirWidth;
    uint32_t u32VirHeight;
    uint32_t u32StreamBufCnt;
    uint32_t u32Resv[7];
} VENC_ATTR_S;

typedef struct {
    VENC_ATTR_S stVencAttr;
    VENC_RC_ATTR_S stRcAttr;
    VENC_GOP_ATTR_S stGopAttr;
} VENC_CHN_ATTR_S;
_Static_assert(sizeof(VENC_CHN_ATTR_S) == 0x88, "unexpected RK MPI VENC_CHN_ATTR_S ABI");

typedef struct {
    int32_t s32RecvPicNum;
} VENC_RECV_PIC_PARAM_S;

typedef struct {
    MB_BLK pMbBlk;
    uint32_t u32Width;
    uint32_t u32Height;
    uint32_t u32VirWidth;
    uint32_t u32VirHeight;
    int32_t enField;
    int32_t enPixelFormat;
    int32_t enVideoFormat;
    int32_t enCompressMode;
    int32_t enDynamicRange;
    int32_t enColorGamut;
    void *pVirAddr[3];
    uint64_t u64PTS;
    uint32_t u32TimeRef;
    uint32_t u32Resv;
} VIDEO_FRAME_S;

typedef struct {
    VIDEO_FRAME_S stVFrame;
    uint32_t u32PoolId;
    uint32_t u32Resv;
} VIDEO_FRAME_INFO_S;

typedef struct {
    MB_BLK pMbBlk;
    uint32_t u32Len;
    uint64_t u64PTS;
    int32_t bFrameEnd;
    int32_t bStreamEnd;
    uint8_t u8Resv[112];
} VENC_PACK_S;
_Static_assert(sizeof(VENC_PACK_S) == 0x88, "unexpected RK MPI VENC_PACK_S ABI");

typedef struct {
    VENC_PACK_S *pstPack;
    uint32_t u32Pad[3];
    uint32_t u32PackCount;
    uint32_t u32Seq;
    uint8_t u8Resv[376];
} VENC_STREAM_S;
_Static_assert(sizeof(VENC_STREAM_S) == 0x190, "unexpected RK MPI VENC_STREAM_S ABI");

/* --- Rockchip audio ABI --- */
typedef struct {
    uint32_t channels;
    uint32_t sampleRate;
    int32_t bitWidth;
} AIO_SOUND_CARD;

typedef struct {
    AIO_SOUND_CARD soundCard;
    int32_t enSamplerate;
    int32_t enBitwidth;
    int32_t enSoundmode;
    uint32_t u32EXFlag;
    uint32_t u32FrmNum;
    uint32_t u32PtNumPerFrm;
    uint32_t u32ChnCnt;
    unsigned char u8CardName[MAX_AUDIO_CARD_NAME];
} AIO_ATTR_S;

typedef struct {
    int32_t s32UsrFrmDepth;
    int32_t enLoopbackMode;
} AI_CHN_PARAM_S;

typedef struct {
    int32_t enMode;
    int32_t enLoopbackMode;
} AO_CHN_PARAM_S;

typedef struct {
    MB_BLK pMbBlk;
    uint32_t u32Len;
    uint64_t u64TimeStamp;
    uint32_t u32Seq;
    int32_t bBypassMbBlk;
} AUDIO_STREAM_S;

typedef struct {
    int32_t enType;
    int32_t enBitwidth;
    uint32_t u32Channels;
    uint32_t u32SampleRate;
    uint32_t u32BitPerCodedSample;
    uint32_t u32Bitrate;
    uint32_t u32Resv[4];
    void *pstResv;
} AENC_ATTR_CODEC_S;

typedef struct {
    int32_t enType;
    uint32_t u32BufCount;
    AENC_ATTR_CODEC_S stCodecAttr;
    uint32_t u32Depth;
} AENC_CHN_ATTR_S;

typedef struct {
    MB_BLK pMbBlk;
    int32_t enBitWidth;
    int32_t enSoundMode;
    uint64_t u64TimeStamp;
    uint32_t u32Seq;
    uint32_t u32Len;
    int32_t bBypassMbBlk;
} AUDIO_FRAME_S;

typedef struct {
    uint8_t *pu8VirAddr;
    uint32_t u32Reserved0;
    uint64_t u64PhyAddr;
    int32_t s32Fd;
    uint32_t u32Reserved1;
    uint64_t u64Size;
    int32_t (*pFreeCB)(void *opaque);
    void *pOpaque;
} MB_EXT_CONFIG_S;

typedef struct {
    int32_t enModId;
    int32_t s32DevId;
    int32_t s32ChnId;
} MPP_CHN_S;

_Static_assert(sizeof(AIO_ATTR_S) == 104, "unexpected RK MPI AIO_ATTR_S ABI");
_Static_assert(sizeof(AUDIO_STREAM_S) == 24, "unexpected RK MPI AUDIO_STREAM_S ABI");
_Static_assert(sizeof(AENC_ATTR_CODEC_S) == 44, "unexpected RK MPI AENC codec ABI");
_Static_assert(sizeof(AENC_CHN_ATTR_S) == 56, "unexpected RK MPI AENC channel ABI");
_Static_assert(sizeof(AUDIO_FRAME_S) == 40, "unexpected RK MPI AUDIO_FRAME_S ABI");
_Static_assert(sizeof(MB_EXT_CONFIG_S) == 40, "unexpected RK MPI MB_EXT_CONFIG_S ABI");

/* --- Rockchip MPI prototypes --- */
extern int RK_MPI_SYS_Init(void);
extern int RK_MPI_SYS_Exit(void);
extern int RK_MPI_SYS_MmzAlloc(MB_BLK *blk, const char *name, const char *opt, uint32_t size);
extern int RK_MPI_SYS_MmzFree(MB_BLK blk);
extern int RK_MPI_SYS_MmzFlushCache(MB_BLK blk, int32_t is_read);
extern void *RK_MPI_MB_Handle2VirAddr(MB_BLK blk);
extern int RK_MPI_SYS_CreateMB(MB_BLK *block, const MB_EXT_CONFIG_S *config);
extern int RK_MPI_MB_ReleaseMB(MB_BLK block);
extern int RK_MPI_SYS_Bind(const MPP_CHN_S *src, const MPP_CHN_S *dst);

extern int RK_MPI_VENC_CreateChn(int32_t chn, const VENC_CHN_ATTR_S *attr);
extern int RK_MPI_VENC_DestroyChn(int32_t chn);
extern int RK_MPI_VENC_StartRecvFrame(int32_t chn, const VENC_RECV_PIC_PARAM_S *param);
extern int RK_MPI_VENC_StopRecvFrame(int32_t chn);
extern int RK_MPI_VENC_SendFrame(int32_t chn, const VIDEO_FRAME_INFO_S *frame, int32_t timeout);
extern int RK_MPI_VENC_GetStream(int32_t chn, VENC_STREAM_S *stream, int32_t timeout);
extern int RK_MPI_VENC_ReleaseStream(int32_t chn, VENC_STREAM_S *stream);

extern int RK_MPI_AI_SetPubAttr(AUDIO_DEV dev, const AIO_ATTR_S *attr);
extern int RK_MPI_AI_Enable(AUDIO_DEV dev);
extern int RK_MPI_AI_Disable(AUDIO_DEV dev);
extern int RK_MPI_AI_SetChnParam(AUDIO_DEV dev, AI_CHN chn, const AI_CHN_PARAM_S *param);
extern int RK_MPI_AI_EnableChn(AUDIO_DEV dev, AI_CHN chn);
extern int RK_MPI_AI_DisableChn(AUDIO_DEV dev, AI_CHN chn);
extern int RK_MPI_AI_SetTrackMode(AUDIO_DEV dev, int32_t mode);
extern int RK_MPI_AENC_CreateChn(AENC_CHN chn, const AENC_CHN_ATTR_S *attr);
extern int RK_MPI_AENC_DestroyChn(AENC_CHN chn);
extern int RK_MPI_AENC_GetStream(AENC_CHN chn, AUDIO_STREAM_S *stream, int32_t timeout_ms);
extern int RK_MPI_AENC_ReleaseStream(AENC_CHN chn, AUDIO_STREAM_S *stream);

extern int RK_MPI_AO_SetPubAttr(AUDIO_DEV dev, const AIO_ATTR_S *attr);
extern int RK_MPI_AO_Enable(AUDIO_DEV dev);
extern int RK_MPI_AO_SetChnParams(AUDIO_DEV dev, AO_CHN chn, const AO_CHN_PARAM_S *params);
extern int RK_MPI_AO_EnableChn(AUDIO_DEV dev, AO_CHN chn);
extern int RK_MPI_AO_EnableReSmp(AUDIO_DEV dev, AO_CHN chn, int32_t input_rate);
extern int RK_MPI_AO_DisableReSmp(AUDIO_DEV dev, AO_CHN chn);
extern int RK_MPI_AO_DisableChn(AUDIO_DEV dev, AO_CHN chn);
extern int RK_MPI_AO_ClearChnBuf(AUDIO_DEV dev, AO_CHN chn);
extern int RK_MPI_AO_SetTrackMode(AUDIO_DEV dev, int32_t mode);
extern int RK_MPI_AO_SetVolume(AUDIO_DEV dev, int32_t volume);
extern int RK_MPI_AO_SendFrame(AUDIO_DEV dev, AO_CHN chn, const AUDIO_FRAME_S *frame, int32_t timeout_ms);

/* --- Rockchip AIQ prototypes --- */
typedef void rk_aiq_sys_ctx_t;
extern int rk_aiq_uapi2_sysctl_enumStaticMetas(int index, void *static_info);
extern rk_aiq_sys_ctx_t *rk_aiq_uapi2_sysctl_init(const char *sns_ent_name,
                                                   const char *iq_file_dir,
                                                   void *err_cb,
                                                   void *metas_cb);
extern int rk_aiq_uapi2_sysctl_prepare(const rk_aiq_sys_ctx_t *ctx,
                                        uint32_t width,
                                        uint32_t height,
                                        int mode);
extern int rk_aiq_uapi2_sysctl_start(const rk_aiq_sys_ctx_t *ctx);
extern int rk_aiq_uapi2_sysctl_stop(const rk_aiq_sys_ctx_t *ctx, int keep_ext_hw_st);
extern void rk_aiq_uapi2_sysctl_deinit(rk_aiq_sys_ctx_t *ctx);
extern int rk_aiq_uapi2_sysctl_switch_scene(const rk_aiq_sys_ctx_t *ctx,
                                            const char *main_scene,
                                            const char *sub_scene);
extern int rk_aiq_uapi2_sysctl_get3AStatsBlk(const rk_aiq_sys_ctx_t *ctx,
                                             void **stats, int timeout_ms);
extern void rk_aiq_uapi2_sysctl_release3AStatsRef(const rk_aiq_sys_ctx_t *ctx,
                                                  void *stats);

/* --- Rockchip SmartIR prototypes --- */
typedef void rk_smart_ir_ctx_t;
extern rk_smart_ir_ctx_t *rk_smart_ir_init(const rk_aiq_sys_ctx_t *aiq_ctx);
extern void rk_smart_ir_deInit(rk_smart_ir_ctx_t *ctx);
extern int rk_smart_ir_runOnce(rk_smart_ir_ctx_t *ctx, void *stats, int *is_night);

#define IRCUT_GPIO_A 130
#define IRCUT_GPIO_B 134
#define IRLED_GPIO   118

/* ------------------------------------------------------------------ */
/* State and synchronization                                           */
/* ------------------------------------------------------------------ */

static volatile sig_atomic_t stopping;

static pthread_mutex_t client_lock = PTHREAD_MUTEX_INITIALIZER;
static int client_fd = -1;
static int listen_fd = -1;

static int sys_initialized;
static int venc_created;
static int venc_started;
static int ai_enabled;
static int ai_channel_enabled;
static int aenc_created;
static int ai_bound;
static int ao_enabled;
static int ao_channel_enabled;
static int speaker_gpio_active;
static int speaker_gpio_exported_here;
static HANDLE_AACDECODER aac_decoder;
static unsigned int current_ao_volume = DEFAULT_AO_VOLUME;
static uint32_t stream_sequence;

/* The 2304x1296 sensor/ISP mode delivers a fixed ~15.17 fps frame period
 * (measured: 4.877 s per 74 frames, unchanged when 20 or 30 fps is requested).
 * The VENC must be told the rate the capture path really produces: CBR divides
 * the bitrate budget by the declared frame rate, so declaring 20 fps while
 * delivering 15.17 starves every frame of bits (12.5 KB/frame = 1545 kbps
 * instead of 17 KB/frame = 2048 kbps). 30 fps is worse still at 1020 kbps. */
#define DEFAULT_VIDEO_FPS 15
#define DEFAULT_VIDEO_BITRATE_KBPS 2048

static const char *socket_path = "/tmp/c220-media.sock";
static const char *video_device = "/dev/video12";
static const char *audio_card = "hw:0,0";
static const char *iq_dir = "/home";
static int video_fps = DEFAULT_VIDEO_FPS;
static int video_bitrate_kbps = DEFAULT_VIDEO_BITRATE_KBPS;

static int video_fd = -1;
static int venc_channel = 1;
static int streaming;
static int aiq_started;
static rk_aiq_sys_ctx_t *aiq_ctx;
static rk_smart_ir_ctx_t *smart_ir_ctx;
static pthread_t night_thread;
static int night_thread_started;
static int current_night_mode = 0;   /* 0 = day, 1 = night */
static int night_mode_target = -1;  /* -1 = auto, 0 = force day, 1 = force night */
static pthread_mutex_t night_lock = PTHREAD_MUTEX_INITIALIZER;

static int write_gpio_file(const char *path, const char *value);
static int send_record(int kind, const unsigned char *payload, size_t length,
                       uint64_t timestamp_us, int keyframe);
static uint64_t next_pts(void);

static int gpio_set_pin(int pin, int value) {
    char path[64];
    snprintf(path, sizeof(path), "/sys/class/gpio/gpio%d", pin);
    if (access(path, F_OK) != 0) {
        int fd = open("/sys/class/gpio/export", O_WRONLY);
        if (fd >= 0) {
            char num[16];
            int n = snprintf(num, sizeof(num), "%d", pin);
            (void)write(fd, num, (size_t)n);
            close(fd);
            usleep(10000);
        }
    }
    snprintf(path, sizeof(path), "/sys/class/gpio/gpio%d/direction", pin);
    (void)write_gpio_file(path, "out");
    snprintf(path, sizeof(path), "/sys/class/gpio/gpio%d/value", pin);
    return write_gpio_file(path, value ? "1" : "0");
}

static void ircut_switch(int is_night) {
    if (is_night) {
        /* Night: remove IR-cut filter (pulse GPIO 130 high for 200ms) */
        gpio_set_pin(IRCUT_GPIO_B, 0);
        gpio_set_pin(IRCUT_GPIO_A, 1);
        usleep(200000);
        gpio_set_pin(IRCUT_GPIO_A, 0);
        gpio_set_pin(IRCUT_GPIO_B, 0);
    } else {
        /* Day: insert IR-cut filter (pulse GPIO 134 high for 200ms) */
        gpio_set_pin(IRCUT_GPIO_A, 0);
        gpio_set_pin(IRCUT_GPIO_B, 1);
        usleep(200000);
        gpio_set_pin(IRCUT_GPIO_A, 0);
        gpio_set_pin(IRCUT_GPIO_B, 0);
    }
}

static void apply_night_mode(int is_night) {
    pthread_mutex_lock(&night_lock);
    if (current_night_mode == is_night) {
        pthread_mutex_unlock(&night_lock);
        return;
    }
    current_night_mode = is_night;
    pthread_mutex_unlock(&night_lock);

    fprintf(stderr, "media_night_mode_switch=%d\n", is_night);

    /* 1. Switch IR-CUT filter */
    ircut_switch(is_night);

    /* 2. Set IR LED */
    gpio_set_pin(IRLED_GPIO, is_night);

    /* 3. Switch AIQ scene */
    if (aiq_ctx) {
        rk_aiq_uapi2_sysctl_switch_scene(aiq_ctx, "normal", is_night ? "night" : "day");
    }

    /* 4. Notify daemon client */
    unsigned char payload[2];
    payload[0] = (unsigned char)(is_night != 0);
    payload[1] = (unsigned char)(night_mode_target < 0 ? 0 : 1);
    (void)send_record(KIND_NIGHT_MODE, payload, sizeof(payload), next_pts(), 0);
}

static void *night_monitor_thread(void *arg) {
    (void)arg;
    int confirm_count = 0;
    while (!stopping) {
        sleep(1);
        if (stopping || !aiq_started || !aiq_ctx || !smart_ir_ctx) continue;

        pthread_mutex_lock(&night_lock);
        int target = night_mode_target;
        int current = current_night_mode;
        pthread_mutex_unlock(&night_lock);

        if (target >= 0) {
            if (current != target) {
                apply_night_mode(target);
            }
            confirm_count = 0;
            continue;
        }

        void *stats = NULL;
        if (rk_aiq_uapi2_sysctl_get3AStatsBlk(aiq_ctx, &stats, 500) == 0 && stats) {
            int detected_night = current;
            rk_smart_ir_runOnce(smart_ir_ctx, stats, &detected_night);
            rk_aiq_uapi2_sysctl_release3AStatsRef(aiq_ctx, stats);

            if (detected_night != current) {
                confirm_count++;
                if (confirm_count >= 2) {
                    apply_night_mode(detected_night);
                    confirm_count = 0;
                    sleep(3);
                }
            } else {
                confirm_count = 0;
            }
        }
    }
    return NULL;
}
static MB_BLK video_input;
static unsigned char *video_input_ptr;
static unsigned char video_frame_buf[MAX_AU_SIZE];

static MB_BLK sub_input;
static unsigned char *sub_input_ptr;
static unsigned char sub_frame_buf[MAX_SUB_AU_SIZE];
static int sub_enabled = 0;
/* Let the VENC scale the 2K frame down itself instead of scaling on the CPU:
 * the software path reads uncached MMZ byte by byte and starved the ISP. */
static int sub_dry;
static int sub_fps = SUB_DEFAULT_FPS;
static int sub_bitrate_kbps = SUB_DEFAULT_BITRATE_KBPS;
static int venc_sub_created;
static int venc_sub_started;

static MPP_CHN_S ai_channel;
static MPP_CHN_S aenc_channel;

static void on_signal(int signal_number) {
    (void)signal_number;
    stopping = 1;
    if (listen_fd >= 0) {
        shutdown(listen_fd, SHUT_RDWR);
        close(listen_fd);
        listen_fd = -1;
    }
}

static void report_error(const char *operation, int result) {
    fprintf(stderr, "media_%s_error=%d (0x%x)\n", operation, result,
            (unsigned int)result);
}

static int write_exact(int fd, const void *buf, size_t len) {
    const unsigned char *p = (const unsigned char *)buf;
    while (len > 0) {
        ssize_t n = write(fd, p, len);
        if (n < 0) {
            if (errno == EINTR) continue;
            return -1;
        }
        if (n == 0) return -1;
        p += n;
        len -= (size_t)n;
    }
    return 0;
}

static int client_current(void) {
    int descriptor;
    pthread_mutex_lock(&client_lock);
    descriptor = client_fd;
    pthread_mutex_unlock(&client_lock);
    return descriptor;
}

static void client_replace(int descriptor) {
    int previous;
    pthread_mutex_lock(&client_lock);
    previous = client_fd;
    client_fd = descriptor;
    pthread_mutex_unlock(&client_lock);
    if (previous >= 0 && previous != descriptor) {
        close(previous);
        fprintf(stderr, "media_client_replaced=1\n");
    }
    if (descriptor >= 0) {
        unsigned char payload[2];
        pthread_mutex_lock(&night_lock);
        payload[0] = (unsigned char)(current_night_mode != 0);
        payload[1] = (unsigned char)(night_mode_target < 0 ? 0 : 1);
        pthread_mutex_unlock(&night_lock);
        (void)send_record(KIND_NIGHT_MODE, payload, sizeof(payload), next_pts(), 0);
    }
}

static int send_record(int kind, const unsigned char *payload, size_t length,
                       uint64_t timestamp_us, int keyframe) {
    unsigned char header[FRAME_HEADER_LEN] = { 'C', '2', '2', '0' };
    int descriptor;
    int result = 0;

    if (length > 0xffffffffU) return -1;
    header[4] = (unsigned char)length;
    header[5] = (unsigned char)(length >> 8);
    header[6] = (unsigned char)(length >> 16);
    header[7] = (unsigned char)(length >> 24);
    for (unsigned int index = 0; index < 8; ++index) {
        header[8 + index] = (unsigned char)(timestamp_us >> (index * 8));
    }
    header[16] = (unsigned char)(keyframe != 0);
    header[17] = (unsigned char)kind;
    header[18] = (unsigned char)(kind == KIND_VIDEO ? CODEC_H265 : 0);

    pthread_mutex_lock(&client_lock);
    descriptor = client_fd;
    if (descriptor >= 0) {
        if (write_exact(descriptor, header, sizeof(header)) != 0 ||
            (length > 0 && write_exact(descriptor, payload, length) != 0)) {
            result = -1;
        }
    } else {
        result = -1;
    }
    pthread_mutex_unlock(&client_lock);
    return result;
}

/* ------------------------------------------------------------------ */
/* Video capture and encode                                            */
/* ------------------------------------------------------------------ */

static int xioctl(int fd, unsigned long request, void *arg) {
    int ret;
    do {
        ret = ioctl(fd, request, arg);
    } while (ret < 0 && errno == EINTR);
    return ret;
}

static uint64_t next_pts(void) {
    static uint64_t pts;
    pts += 50000;
    return pts;
}

static uint64_t capture_pts_us(const struct v4l2_buffer *buffer) {
    if (buffer->timestamp.tv_sec >= 0 && buffer->timestamp.tv_usec >= 0) {
        uint64_t seconds = (uint64_t)buffer->timestamp.tv_sec;
        uint64_t micros = (uint64_t)buffer->timestamp.tv_usec;
        if (seconds || micros) return seconds * 1000000ULL + micros;
    }
    return next_pts();
}

static int queue_input(int fd, int index, void *ptr, size_t size) {
    struct v4l2_buffer b;
    struct v4l2_plane p[VIDEO_MAX_PLANES];
    memset(&b, 0, sizeof(b));
    memset(p, 0, sizeof(p));
    b.index = (uint32_t)index;
    b.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
    b.memory = V4L2_MEMORY_USERPTR;
    b.length = 1;
    b.m.planes = p;
    p[0].m.userptr = (unsigned long)(uintptr_t)ptr;
    p[0].length = (unsigned int)size;
    return xioctl(fd, VIDIOC_QBUF, &b);
}

static int start_aiq(void) {
    unsigned char static_info[4096];
    char sensor_name[33];
    int result;

    if (setenv("HDR_MODE", "0", 1) != 0) {
        fprintf(stderr, "media_aiq_setenv_error=%s\n", strerror(errno));
        return -1;
    }
    memset(static_info, 0, sizeof(static_info));
    result = rk_aiq_uapi2_sysctl_enumStaticMetas(0, static_info);
    memcpy(sensor_name, static_info, 32);
    sensor_name[32] = '\0';
    fprintf(stderr, "media_aiq_enum=%d sensor='%s'\n", result, sensor_name);
    if (result != 0 || sensor_name[0] == '\0') return -1;

    aiq_ctx = rk_aiq_uapi2_sysctl_init(sensor_name, iq_dir, NULL, NULL);
    if (!aiq_ctx) { report_error("aiq_init", -1); return -1; }
    result = rk_aiq_uapi2_sysctl_prepare(aiq_ctx, WIDTH, HEIGHT, 0);
    if (result != 0) { report_error("aiq_prepare", result); return -1; }
    result = rk_aiq_uapi2_sysctl_start(aiq_ctx);
    if (result != 0) { report_error("aiq_start", result); return -1; }
    aiq_started = 1;
    smart_ir_ctx = rk_smart_ir_init(aiq_ctx);
    if (smart_ir_ctx) {
        fprintf(stderr, "media_smart_ir_ready=1\n");
    } else {
        fprintf(stderr, "media_smart_ir_init_failed=1\n");
    }
    ircut_switch(0);
    gpio_set_pin(IRLED_GPIO, 0);
    return 0;
}

static void stop_aiq(void) {
    if (smart_ir_ctx) {
        rk_smart_ir_deInit(smart_ir_ctx);
        smart_ir_ctx = NULL;
    }
    if (aiq_started && aiq_ctx) {
        rk_aiq_uapi2_sysctl_stop(aiq_ctx, 0);
        aiq_started = 0;
    }
    if (aiq_ctx) {
        rk_aiq_uapi2_sysctl_deinit(aiq_ctx);
        aiq_ctx = NULL;
    }
    gpio_set_pin(IRLED_GPIO, 0);
    gpio_set_pin(IRCUT_GPIO_A, 0);
    gpio_set_pin(IRCUT_GPIO_B, 0);
}

static int open_video(void) {
    struct v4l2_capability cap;
    struct v4l2_format fmt;
    struct v4l2_requestbuffers req;
    enum v4l2_buf_type type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
    VENC_CHN_ATTR_S attr;
    VENC_RECV_PIC_PARAM_S recv;
    uint32_t gop;
    RK_S32 result;

    video_fd = open(video_device, O_RDWR | O_NONBLOCK | O_CLOEXEC);
    if (video_fd < 0) {
        fprintf(stderr, "media_video_open_error=%s\n", strerror(errno));
        return -1;
    }
    memset(&cap, 0, sizeof(cap));
    if (xioctl(video_fd, VIDIOC_QUERYCAP, &cap) < 0) {
        report_error("video_querycap", -1);
        return -1;
    }
    __u32 caps = (cap.capabilities & V4L2_CAP_DEVICE_CAPS) ? cap.device_caps
                                                           : cap.capabilities;
    if (!(caps & V4L2_CAP_VIDEO_CAPTURE_MPLANE) || !(caps & V4L2_CAP_STREAMING)) {
        fprintf(stderr, "media_video_caps=0x%08x\n", caps);
        return -1;
    }
    memset(&fmt, 0, sizeof(fmt));
    fmt.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
    if (xioctl(video_fd, VIDIOC_G_FMT, &fmt) < 0) {
        report_error("video_g_fmt", -1);
        return -1;
    }
    if (fmt.fmt.pix_mp.width != WIDTH || fmt.fmt.pix_mp.height != HEIGHT ||
        fmt.fmt.pix_mp.pixelformat != V4L2_PIX_FMT_NV12 ||
        fmt.fmt.pix_mp.num_planes != 1) {
        fprintf(stderr, "media_video_format_unsupported=1\n");
        return -1;
    }

    memset(&attr, 0, sizeof(attr));
    attr.stVencAttr.u32MaxPicWidth = WIDTH;
    attr.stVencAttr.u32MaxPicHeight = HEIGHT;
    attr.stVencAttr.enPixelFormat = RK_FMT_YUV420SP;
    attr.stVencAttr.u32BufSize = 1024U * 1024U;
    attr.stVencAttr.bByFrame = RK_TRUE;
    attr.stVencAttr.u32PicWidth = WIDTH;
    attr.stVencAttr.u32PicHeight = HEIGHT;
    attr.stVencAttr.u32VirWidth = WIDTH;
    attr.stVencAttr.u32VirHeight = HEIGHT;
    attr.stVencAttr.u32StreamBufCnt = 2;
    gop = (uint32_t)video_fps * 2;
    attr.stVencAttr.enType = RK_VIDEO_ID_HEVC;
    attr.stVencAttr.u32Profile = H265E_PROFILE_MAIN;
    attr.stRcAttr.enRcMode = VENC_RC_MODE_H265CBR;
    attr.stRcAttr.stH265Cbr.u32Gop = gop;
    attr.stRcAttr.stH265Cbr.u32SrcFrameRateNum = (uint32_t)video_fps;
    attr.stRcAttr.stH265Cbr.u32SrcFrameRateDen = 1;
    attr.stRcAttr.stH265Cbr.fr32DstFrameRateNum = (uint32_t)video_fps;
    attr.stRcAttr.stH265Cbr.fr32DstFrameRateDen = 1;
    attr.stRcAttr.stH265Cbr.u32BitRate = (uint32_t)video_bitrate_kbps;
    attr.stRcAttr.stH265Cbr.u32StatTime = 1;
    attr.stGopAttr.enGopMode = VENC_GOPMODE_NORMALP;

    result = RK_MPI_VENC_CreateChn(venc_channel, &attr);
    if (result != RK_SUCCESS) { report_error("venc_create", result); return -1; }
    venc_created = 1;

    memset(&recv, 0, sizeof(recv));
    recv.s32RecvPicNum = -1;
    result = RK_MPI_VENC_StartRecvFrame(venc_channel, &recv);
    if (result != RK_SUCCESS) { report_error("venc_start", result); return -1; }
    venc_started = 1;

    if (start_aiq() != 0) return -1;

    memset(&req, 0, sizeof(req));
    req.count = NUM_INPUT_BUFFERS;
    req.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
    req.memory = V4L2_MEMORY_USERPTR;
    if (xioctl(video_fd, VIDIOC_REQBUFS, &req) < 0 ||
        req.count < NUM_INPUT_BUFFERS) {
        report_error("video_reqbufs", -1);
        return -1;
    }
    if (queue_input(video_fd, 0, video_input_ptr, FRAME_SIZE) < 0) {
        report_error("video_qbuf", -1);
        return -1;
    }
    if (xioctl(video_fd, VIDIOC_STREAMON, &type) < 0) {
        report_error("video_streamon", -1);
        return -1;
    }
    streaming = 1;

    /* Report the frame interval the sensor/ISP mode actually provides, then ask
     * for the configured rate so the VENC cadence matches the capture rate.
     * A mismatch makes CBR budget bits per frame for a rate the hardware never
     * delivers, which silently lowers the stream bitrate. */
    {
        struct v4l2_streamparm parm;
        memset(&parm, 0, sizeof(parm));
        parm.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        if (xioctl(video_fd, VIDIOC_G_PARM, &parm) == 0) {
            fprintf(stderr, "media_capture_interval=%u/%u caps=0x%08x\n",
                    parm.parm.capture.timeperframe.numerator,
                    parm.parm.capture.timeperframe.denominator,
                    parm.parm.capture.capability);
        }
        parm.parm.capture.timeperframe.numerator = 1;
        parm.parm.capture.timeperframe.denominator = (uint32_t)video_fps;
        if (xioctl(video_fd, VIDIOC_S_PARM, &parm) == 0) {
            fprintf(stderr, "media_capture_requested=%d result=%u/%u\n", video_fps,
                    parm.parm.capture.timeperframe.numerator,
                    parm.parm.capture.timeperframe.denominator);
        } else {
            fprintf(stderr, "media_capture_request_failed=%s\n", strerror(errno));
        }
    }

    fprintf(stderr, "media_video_ready=h265 %ux%u fps=%d bitrate=%d\n", WIDTH, HEIGHT,
            video_fps, video_bitrate_kbps);
    return 0;
}

static void close_video(void) {
    enum v4l2_buf_type type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
    if (streaming && video_fd >= 0) {
        xioctl(video_fd, VIDIOC_STREAMOFF, &type);
        streaming = 0;
    }
    if (video_fd >= 0) {
        struct v4l2_requestbuffers zero;
        memset(&zero, 0, sizeof(zero));
        zero.type = type;
        zero.memory = V4L2_MEMORY_USERPTR;
        xioctl(video_fd, VIDIOC_REQBUFS, &zero);
        close(video_fd);
        video_fd = -1;
    }
    if (venc_started) { RK_MPI_VENC_StopRecvFrame(venc_channel); venc_started = 0; }
    if (venc_created) { RK_MPI_VENC_DestroyChn(venc_channel); venc_created = 0; }
    stop_aiq();
}

/* HEVC IRAP pictures (BLA/IDR/CRA, NAL types 16..23) start a new group. */
static int detect_keyframe(const unsigned char *data, size_t length) {
    for (size_t index = 0; index + 4 < length; ++index) {
        size_t offset = 0;
        if (data[index] == 0 && data[index + 1] == 0 && data[index + 2] == 1) {
            offset = index + 3;
        } else if (data[index] == 0 && data[index + 1] == 0 &&
                   data[index + 2] == 0 && data[index + 3] == 1) {
            offset = index + 4;
        }
        if (offset > 0 && offset < length) {
            uint8_t nal_type = (data[offset] >> 1) & 0x3f;
            if (nal_type >= 16 && nal_type <= 23) return 1;
        }
    }
    return 0;
}

/* Area-average NV12 downscale from the 2K capture buffer to 640x360.
 *
 * Both axes scale by exactly 18/5 = 3.6, so one fixed-point phase serves rows
 * and columns. Each output pixel averages an overlapping 4x4 source block and
 * divides by 16 with a shift: this binary links uClibc with -nostdlib and has
 * no libgcc, so a runtime division would need __aeabi_uidiv, which does not
 * exist here. Division by the constant 5 is folded into a multiply by the
 * compiler. Reading a 4x4 block per pixel costs about 1.2 frame reads, and the
 * four rows are touched in sequence, so the DRAM pattern stays row-major.
 */
static void downscale_nv12(const unsigned char *src, unsigned char *dst) {
    const unsigned char *src_uv = src + (size_t)WIDTH * HEIGHT;
    unsigned char *dst_uv = dst + (size_t)SUB_WIDTH * SUB_HEIGHT;

    for (unsigned int y = 0; y < SUB_HEIGHT; ++y) {
        unsigned int sy = (y * 18U) / 5U;
        const unsigned char *row0 = src + (size_t)sy * WIDTH;
        const unsigned char *row1 = row0 + WIDTH;
        const unsigned char *row2 = row1 + WIDTH;
        const unsigned char *row3 = row2 + WIDTH;
        unsigned char *out = dst + (size_t)y * SUB_WIDTH;

        for (unsigned int x = 0; x < SUB_WIDTH; ++x) {
            unsigned int sx = (x * 18U) / 5U;
            unsigned int sum = row0[sx] + row0[sx + 1] + row0[sx + 2] + row0[sx + 3] +
                               row1[sx] + row1[sx + 1] + row1[sx + 2] + row1[sx + 3] +
                               row2[sx] + row2[sx + 1] + row2[sx + 2] + row2[sx + 3] +
                               row3[sx] + row3[sx + 1] + row3[sx + 2] + row3[sx + 3];
            out[x] = (unsigned char)(sum >> 4);
        }
    }

    /* Interleaved chroma. The chroma plane is half resolution in both axes, so
     * the horizontal source spacing is 1.8 samples, not the luma 3.6, and a
     * tap pair covers one output sample. Taking four taps here would read past
     * the end of the row, so the box is 2 samples wide by 4 rows and divides by
     * 8. Row count is unchanged at 3.6:1. */
    for (unsigned int y = 0; y < SUB_HEIGHT / 2U; ++y) {
        unsigned int sy = (y * 18U) / 5U;
        const unsigned char *row0 = src_uv + (size_t)sy * WIDTH;
        const unsigned char *row1 = row0 + WIDTH;
        const unsigned char *row2 = row1 + WIDTH;
        const unsigned char *row3 = row2 + WIDTH;
        unsigned char *out = dst_uv + (size_t)y * SUB_WIDTH;

        for (unsigned int x = 0; x < SUB_WIDTH; ++x) {
            /* Chroma sample index: 1.8 samples per output pixel, so 18/10. */
            unsigned int sx = ((x * 18U) / 10U) * 2U;
            unsigned int u = row0[sx] + row0[sx + 2] + row1[sx] + row1[sx + 2] +
                             row2[sx] + row2[sx + 2] + row3[sx] + row3[sx + 2];
            unsigned int v = row0[sx + 1] + row0[sx + 3] + row1[sx + 1] + row1[sx + 3] +
                             row2[sx + 1] + row2[sx + 3] + row3[sx + 1] + row3[sx + 3];
            out[x * 2U] = (unsigned char)(u >> 3);
            out[x * 2U + 1U] = (unsigned char)(v >> 3);
        }
    }
}

/* Create and start the sub-stream encoder channel. Returns 0 on success; a
 * failure disables only the sub-stream so the main stream keeps running. */
static int open_sub_encoder(void) {
    VENC_CHN_ATTR_S attr;
    VENC_RECV_PIC_PARAM_S recv;
    uint32_t gop = (uint32_t)sub_fps * 2U;
    RK_S32 result;

    if (!sub_enabled) return 0;
    {
        if (RK_MPI_SYS_MmzAlloc(&sub_input, NULL, NULL, (RK_U32)SUB_FRAME_SIZE) !=
                RK_SUCCESS ||
            !sub_input) {
            report_error("sub_mmz", -1);
            sub_enabled = 0;
            return -1;
        }
        sub_input_ptr = (unsigned char *)(unsigned char *)RK_MPI_MB_Handle2VirAddr(sub_input);
    }

    memset(&attr, 0, sizeof(attr));
    attr.stVencAttr.enType = RK_VIDEO_ID_HEVC;
    /* MaxPic describes the input the encoder may receive; Pic the output it
     * produces. When they differ the encoder scales, so the sub-stream can be
     * fed the 2K capture buffer directly. */
    attr.stVencAttr.u32MaxPicWidth = SUB_WIDTH;
    attr.stVencAttr.u32MaxPicHeight = SUB_HEIGHT;
    attr.stVencAttr.enPixelFormat = RK_FMT_YUV420SP;
    attr.stVencAttr.u32BufSize = SUB_STREAM_BUF_SIZE;
    attr.stVencAttr.bByFrame = RK_TRUE;
    attr.stVencAttr.u32PicWidth = SUB_WIDTH;
    attr.stVencAttr.u32PicHeight = SUB_HEIGHT;
    attr.stVencAttr.u32VirWidth = SUB_WIDTH;
    attr.stVencAttr.u32VirHeight = SUB_HEIGHT;
    attr.stVencAttr.u32StreamBufCnt = 2;
    attr.stVencAttr.u32Profile = H265E_PROFILE_MAIN;
    attr.stRcAttr.enRcMode = VENC_RC_MODE_H265CBR;
    attr.stRcAttr.stH265Cbr.u32Gop = gop;
    attr.stRcAttr.stH265Cbr.u32SrcFrameRateNum = (uint32_t)sub_fps;
    attr.stRcAttr.stH265Cbr.u32SrcFrameRateDen = 1;
    attr.stRcAttr.stH265Cbr.fr32DstFrameRateNum = (uint32_t)sub_fps;
    attr.stRcAttr.stH265Cbr.fr32DstFrameRateDen = 1;
    attr.stRcAttr.stH265Cbr.u32BitRate = (uint32_t)sub_bitrate_kbps;
    attr.stRcAttr.stH265Cbr.u32StatTime = 1;
    attr.stGopAttr.enGopMode = VENC_GOPMODE_NORMALP;

    result = RK_MPI_VENC_CreateChn(VENC_SUB_CHANNEL, &attr);
    if (result != RK_SUCCESS) {
        report_error("sub_venc_create", result);
        sub_enabled = 0;
        return -1;
    }
    venc_sub_created = 1;
    memset(&recv, 0, sizeof(recv));
    recv.s32RecvPicNum = -1;
    result = RK_MPI_VENC_StartRecvFrame(VENC_SUB_CHANNEL, &recv);
    if (result != RK_SUCCESS) {
        report_error("sub_venc_start", result);
        return -1;
    }
    venc_sub_started = 1;
    fprintf(stderr, "media_sub_ready=%ux%u fps=%d bitrate=%d scale=%s\n", SUB_WIDTH,
            SUB_HEIGHT, sub_fps, sub_bitrate_kbps, "cpu");
    return 0;
}

static void close_sub_encoder(void) {
    if (venc_sub_started) {
        RK_MPI_VENC_StopRecvFrame(VENC_SUB_CHANNEL);
        venc_sub_started = 0;
    }
    if (venc_sub_created) {
        RK_MPI_VENC_DestroyChn(VENC_SUB_CHANNEL);
        venc_sub_created = 0;
    }
    if (sub_input) {
        RK_MPI_SYS_MmzFree(sub_input);
        sub_input = NULL;
        sub_input_ptr = NULL;
    }
}

/* Copy the captured frame into the 640x360 buffer. Must run *before* the
 * capture buffer goes back into the V4L2 queue, or the ISP can overwrite it
 * while we read. */
static void downscale_sub_frame(void) {
    if (!sub_enabled || !venc_sub_started) return;
    /* The ISP wrote this buffer by DMA, so drop any stale CPU cache lines
     * before reading it, then hand the scaled copy to the encoder by DMA. */
    RK_MPI_SYS_MmzFlushCache(video_input, RK_TRUE);
    downscale_nv12(video_input_ptr, sub_input_ptr);
    RK_MPI_SYS_MmzFlushCache(sub_input, RK_FALSE);
}

/* Encode the already scaled frame and hand it to the daemon. Runs after the
 * capture buffer is back in the V4L2 queue, so the ISP never waits for it. */
static void encode_sub_frame(uint64_t pts_us, unsigned long *sub_frames) {
    VIDEO_FRAME_INFO_S info;
    VENC_PACK_S packs[4];
    VENC_STREAM_S stream;
    size_t total = 0;
    unsigned int count;
    RK_S32 result;

    if (!sub_enabled || !venc_sub_started || sub_dry) return;

    memset(&info, 0, sizeof(info));
    info.stVFrame.pMbBlk = sub_input;
    info.stVFrame.u32Width = SUB_WIDTH;
    info.stVFrame.u32Height = SUB_HEIGHT;
    info.stVFrame.u32VirWidth = SUB_WIDTH;
    info.stVFrame.u32VirHeight = SUB_HEIGHT;
    info.stVFrame.enField = 4;
    info.stVFrame.enPixelFormat = RK_FMT_YUV420SP;
    info.stVFrame.pVirAddr[0] = sub_input_ptr;
    info.stVFrame.pVirAddr[1] = sub_input_ptr + (size_t)SUB_WIDTH * SUB_HEIGHT;
    info.stVFrame.u64PTS = pts_us;

    result = RK_MPI_VENC_SendFrame(VENC_SUB_CHANNEL, &info, 1000);
    if (result != RK_SUCCESS) {
        report_error("sub_send", result);
        return;
    }

    memset(packs, 0, sizeof(packs));
    memset(&stream, 0, sizeof(stream));
    stream.pstPack = packs;
    stream.u32PackCount = 4;
    result = RK_MPI_VENC_GetStream(VENC_SUB_CHANNEL, &stream, 1000);
    if (result != RK_SUCCESS) {
        report_error("sub_get_stream", result);
        return;
    }

    count = stream.u32PackCount > 4 ? 4 : stream.u32PackCount;
    for (unsigned int index = 0; index < count; ++index) {
        size_t length = packs[index].u32Len;
        unsigned char *source;
        if (total + length > MAX_SUB_AU_SIZE) break;
        source = (unsigned char *)RK_MPI_MB_Handle2VirAddr(packs[index].pMbBlk);
        RK_MPI_SYS_MmzFlushCache(packs[index].pMbBlk, RK_TRUE);
        if (source) {
            memcpy(sub_frame_buf + total, source, length);
            total += length;
        }
    }
    RK_MPI_VENC_ReleaseStream(VENC_SUB_CHANNEL, &stream);

    if (total > 0 && client_current() >= 0) {
        (*sub_frames)++;
        (void)send_record(KIND_VIDEO_SUB, sub_frame_buf, total, pts_us,
                          detect_keyframe(sub_frame_buf, total));
    }
}

static void *video_thread(void *argument) {
    unsigned long captured = 0, sent_bytes = 0, empty_polls = 0, short_frames = 0;
    unsigned long send_errors = 0, stream_errors = 0, loop_iterations = 0;
    unsigned long long pts_first_us = 0;
    unsigned long sub_frames = 0;
    /* Encode every Nth captured frame: the capture path runs at ~15 fps, so a
     * divisor of 3 yields the 5 fps that detection wants. */
    unsigned long sub_divisor = (unsigned long)(DEFAULT_VIDEO_FPS / SUB_DEFAULT_FPS);
    int sub_due = 0;

    (void)argument;
    if (open_video() != 0) return NULL;
    if (sub_divisor < 1) sub_divisor = 1;
    if (open_sub_encoder() != 0) {
        fprintf(stderr, "media_sub_disabled=1\n");
    }

    while (!stopping) {
        struct pollfd pfd;
        struct v4l2_buffer buffer;
        struct v4l2_plane planes[VIDEO_MAX_PLANES];
        VIDEO_FRAME_INFO_S info;
        VENC_PACK_S packs[8];
        VENC_STREAM_S stream;
        uint64_t pts_us;
        size_t total = 0;
        unsigned int count;
        RK_S32 result;

        memset(&pfd, 0, sizeof(pfd));
        pfd.fd = video_fd;
        pfd.events = POLLIN;
        if (poll(&pfd, 1, 3000) <= 0) {
            empty_polls++;
            continue;
        }

        memset(&buffer, 0, sizeof(buffer));
        memset(planes, 0, sizeof(planes));
        buffer.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        buffer.memory = V4L2_MEMORY_USERPTR;
        buffer.length = 1;
        buffer.m.planes = planes;
        if (xioctl(video_fd, VIDIOC_DQBUF, &buffer) < 0) {
            empty_polls++;
            continue;
        }
        if (planes[0].bytesused < FRAME_SIZE) {
            short_frames++;
            queue_input(video_fd, 0, video_input_ptr, FRAME_SIZE);
            continue;
        }

        sub_due = sub_enabled && venc_sub_started && (captured % sub_divisor) == 0UL;
        if (sub_due) {
            downscale_sub_frame();
        }

        RK_MPI_SYS_MmzFlushCache(video_input, RK_FALSE);
        memset(&info, 0, sizeof(info));
        info.stVFrame.pMbBlk = video_input;
        info.stVFrame.u32Width = WIDTH;
        info.stVFrame.u32Height = HEIGHT;
        info.stVFrame.u32VirWidth = WIDTH;
        info.stVFrame.u32VirHeight = HEIGHT;
        info.stVFrame.enField = 4;
        info.stVFrame.enPixelFormat = RK_FMT_YUV420SP;
        info.stVFrame.pVirAddr[0] = video_input_ptr;
        info.stVFrame.pVirAddr[1] = video_input_ptr + WIDTH * HEIGHT;
        pts_us = capture_pts_us(&buffer);
        info.stVFrame.u64PTS = pts_us;

        result = RK_MPI_VENC_SendFrame(venc_channel, &info, 2000);
        if (result != RK_SUCCESS) {
            send_errors++;
            queue_input(video_fd, 0, video_input_ptr, FRAME_SIZE);
            usleep(2000);
            continue;
        }

        memset(packs, 0, sizeof(packs));
        memset(&stream, 0, sizeof(stream));
        stream.pstPack = packs;
        stream.u32PackCount = 8;
        result = RK_MPI_VENC_GetStream(venc_channel, &stream, 3000);
        queue_input(video_fd, 0, video_input_ptr, FRAME_SIZE);
        if (result != RK_SUCCESS) {
            stream_errors++;
            continue;
        }

        count = stream.u32PackCount > 8 ? 8 : stream.u32PackCount;
        for (unsigned int index = 0; index < count; ++index) {
            size_t length = packs[index].u32Len;
            unsigned char *source;
            if (total + length > MAX_AU_SIZE) break;
            source = (unsigned char *)RK_MPI_MB_Handle2VirAddr(packs[index].pMbBlk);
            RK_MPI_SYS_MmzFlushCache(packs[index].pMbBlk, RK_TRUE);
            if (source) {
                memcpy(video_frame_buf + total, source, length);
                total += length;
            }
        }
        RK_MPI_VENC_ReleaseStream(venc_channel, &stream);

        if (total > 0) {
            captured++;
            sent_bytes += total;
            if (pts_first_us == 0) pts_first_us = pts_us;
            if (client_current() >= 0) {
                (void)send_record(KIND_VIDEO, video_frame_buf, total, pts_us,
                                  detect_keyframe(video_frame_buf, total));
            }
        }

        if (sub_due) {
            encode_sub_frame(pts_us, &sub_frames);
        }

        if (++loop_iterations % 75 == 0) {
            fprintf(stderr,
                    "media_capture_stats captured=%lu empty=%lu short=%lu send_err=%lu "
                    "stream_err=%lu bytes=%lu sub=%lu pts_first_us=%llu pts_last_us=%llu\n",
                    captured, empty_polls, short_frames, send_errors, stream_errors,
                    sent_bytes, sub_frames, pts_first_us, pts_us);
        }
    }

    close_sub_encoder();
    close_video();
    return NULL;
}

/* ------------------------------------------------------------------ */
/* Microphone capture                                                  */
/* ------------------------------------------------------------------ */

static int open_audio_capture(void) {
    AIO_ATTR_S ai_attr;
    AI_CHN_PARAM_S channel_param;
    AENC_CHN_ATTR_S aenc_attr;
    int result;

    memset(&ai_attr, 0, sizeof(ai_attr));
    ai_attr.soundCard.channels = 2;
    ai_attr.soundCard.sampleRate = AUDIO_SAMPLE_RATE_8000;
    ai_attr.soundCard.bitWidth = AUDIO_BIT_WIDTH_16;
    ai_attr.enSamplerate = AUDIO_SAMPLE_RATE_8000;
    ai_attr.enBitwidth = AUDIO_BIT_WIDTH_16;
    ai_attr.enSoundmode = AUDIO_SOUND_MODE_MONO;
    ai_attr.u32FrmNum = 4;
    ai_attr.u32PtNumPerFrm = AUDIO_FRAME_SAMPLES;
    ai_attr.u32ChnCnt = 2;
    snprintf((char *)ai_attr.u8CardName, sizeof(ai_attr.u8CardName), "%s",
             audio_card);

    result = RK_MPI_AI_SetPubAttr(0, &ai_attr);
    if (result != RK_SUCCESS) { report_error("ai_set_pub_attr", result); return -1; }
    result = RK_MPI_AI_Enable(0);
    if (result != RK_SUCCESS) { report_error("ai_enable", result); return -1; }
    ai_enabled = 1;

    memset(&channel_param, 0, sizeof(channel_param));
    channel_param.s32UsrFrmDepth = 1;
    channel_param.enLoopbackMode = AUDIO_LOOPBACK_NONE;
    result = RK_MPI_AI_SetChnParam(0, 0, &channel_param);
    if (result != RK_SUCCESS) { report_error("ai_set_chn_param", result); return -1; }
    result = RK_MPI_AI_SetTrackMode(0, AUDIO_TRACK_FRONT_LEFT);
    if (result != RK_SUCCESS) { report_error("ai_set_track", result); return -1; }
    result = RK_MPI_AI_EnableChn(0, 0);
    if (result != RK_SUCCESS) { report_error("ai_enable_chn", result); return -1; }
    ai_channel_enabled = 1;

    memset(&aenc_attr, 0, sizeof(aenc_attr));
    aenc_attr.enType = RK_AUDIO_ID_PCM_ALAW;
    aenc_attr.u32BufCount = 4;
    aenc_attr.stCodecAttr.enType = RK_AUDIO_ID_PCM_ALAW;
    aenc_attr.stCodecAttr.enBitwidth = AUDIO_BIT_WIDTH_16;
    aenc_attr.stCodecAttr.u32Channels = 1;
    aenc_attr.stCodecAttr.u32SampleRate = AUDIO_SAMPLE_RATE_8000;
    aenc_attr.stCodecAttr.u32BitPerCodedSample = 8;
    aenc_attr.stCodecAttr.u32Bitrate = 64000;
    aenc_attr.u32Depth = 4;
    result = RK_MPI_AENC_CreateChn(0, &aenc_attr);
    if (result != RK_SUCCESS) { report_error("aenc_create", result); return -1; }
    aenc_created = 1;

    ai_channel.enModId = RK_ID_AI;
    ai_channel.s32DevId = 0;
    ai_channel.s32ChnId = 0;
    aenc_channel.enModId = RK_ID_AENC;
    aenc_channel.s32DevId = 0;
    aenc_channel.s32ChnId = 0;
    result = RK_MPI_SYS_Bind(&ai_channel, &aenc_channel);
    if (result != RK_SUCCESS) { report_error("ai_bind", result); return -1; }
    ai_bound = 1;

    fprintf(stderr, "media_audio_ready=pcma/8000/mono frame_samples=%d card=%s\n",
            AUDIO_FRAME_SAMPLES, audio_card);
    return 0;
}

static void close_audio_capture(void) {
    if (aenc_created) { RK_MPI_AENC_DestroyChn(0); aenc_created = 0; }
    if (ai_channel_enabled) { RK_MPI_AI_DisableChn(0, 0); ai_channel_enabled = 0; }
    if (ai_enabled) { RK_MPI_AI_Disable(0); ai_enabled = 0; }
    ai_bound = 0;
}

static uint64_t audio_fallback_us;

static void *audio_thread(void *argument) {
    (void)argument;
    if (open_audio_capture() != 0) return NULL;

    while (!stopping) {
        AUDIO_STREAM_S stream;
        void *address;
        uint32_t length;
        uint64_t timestamp;
        int result;

        memset(&stream, 0, sizeof(stream));
        result = RK_MPI_AENC_GetStream(0, &stream, 1000);
        if (result != RK_SUCCESS) continue;

        length = stream.u32Len;
        address = RK_MPI_MB_Handle2VirAddr(stream.pMbBlk);
        timestamp = stream.u64TimeStamp;
        if (!timestamp) {
            timestamp = audio_fallback_us;
            audio_fallback_us += 20000;
        }
        if (address && length > 0 && length <= AUDIO_MAX_PAYLOAD &&
            client_current() >= 0) {
            (void)send_record(KIND_AUDIO_CAPTURE, (const unsigned char *)address,
                              length, timestamp, 0);
        }
        RK_MPI_AENC_ReleaseStream(0, &stream);
    }

    close_audio_capture();
    return NULL;
}

/* ------------------------------------------------------------------ */
/* Speaker playback                                                   */
/* ------------------------------------------------------------------ */

static int write_gpio_file(const char *path, const char *value) {
    int descriptor = open(path, O_WRONLY);
    size_t length = strlen(value);
    ssize_t written;
    if (descriptor < 0) return -1;
    written = write(descriptor, value, length);
    close(descriptor);
    return written == (ssize_t)length ? 0 : -1;
}

static int speaker_gpio_set(int enabled) {
    char gpio_path[64];
    int descriptor;

    snprintf(gpio_path, sizeof(gpio_path), "/sys/class/gpio/gpio%d", SPEAKER_GPIO);
    if (access(gpio_path, F_OK) != 0) {
        descriptor = open("/sys/class/gpio/export", O_WRONLY);
        if (descriptor < 0) return -1;
        if (write(descriptor, "102", 3) != 3) { close(descriptor); return -1; }
        close(descriptor);
        speaker_gpio_exported_here = 1;
        usleep(10000);
    }
    snprintf(gpio_path, sizeof(gpio_path),
             "/sys/class/gpio/gpio%d/direction", SPEAKER_GPIO);
    if (write_gpio_file(gpio_path, "out") != 0) return -1;
    snprintf(gpio_path, sizeof(gpio_path), "/sys/class/gpio/gpio%d/value",
             SPEAKER_GPIO);
    if (write_gpio_file(gpio_path, enabled ? "1" : "0") != 0) return -1;
    speaker_gpio_active = enabled;
    return 0;
}

static void speaker_gpio_cleanup(void) {
    if (speaker_gpio_active) (void)speaker_gpio_set(0);
    if (speaker_gpio_exported_here) {
        (void)write_gpio_file("/sys/class/gpio/unexport", "102");
        speaker_gpio_exported_here = 0;
    }
}

static int open_aac_decoder(void) {
    aac_decoder = aacDecoder_Open(TT_MP4_ADTS, 1);
    return aac_decoder ? 0 : -1;
}

static int init_speaker(void) {
    AIO_ATTR_S ao_attr;
    AO_CHN_PARAM_S ao_params;
    int result;

    memset(&ao_attr, 0, sizeof(ao_attr));
    ao_attr.soundCard.channels = 2;
    ao_attr.soundCard.sampleRate = AUDIO_SAMPLE_RATE_8000;
    ao_attr.soundCard.bitWidth = AUDIO_BIT_WIDTH_16;
    ao_attr.enSamplerate = AUDIO_SAMPLE_RATE_8000;
    ao_attr.enBitwidth = AUDIO_BIT_WIDTH_16;
    ao_attr.enSoundmode = AUDIO_SOUND_MODE_MONO;
    ao_attr.u32FrmNum = 4;
    ao_attr.u32PtNumPerFrm = 1024;
    ao_attr.u32ChnCnt = 2;
    snprintf((char *)ao_attr.u8CardName, sizeof(ao_attr.u8CardName), "%s",
             audio_card);

    if (speaker_gpio_set(0) != 0) { report_error("speaker_gpio_init", -1); return -1; }
    result = RK_MPI_AO_SetPubAttr(0, &ao_attr);
    if (result != RK_SUCCESS) { report_error("ao_set_pub_attr", result); return -1; }
    result = RK_MPI_AO_Enable(0);
    if (result != RK_SUCCESS) { report_error("ao_enable", result); return -1; }
    ao_enabled = 1;

    memset(&ao_params, 0, sizeof(ao_params));
    ao_params.enMode = AUDIO_CHN_MODE_DEFAULT;
    ao_params.enLoopbackMode = AUDIO_LOOPBACK_NONE;
    result = RK_MPI_AO_SetChnParams(0, 0, &ao_params);
    if (result != RK_SUCCESS) { report_error("ao_set_chn_params", result); return -1; }
    result = RK_MPI_AO_EnableChn(0, 0);
    if (result != RK_SUCCESS) { report_error("ao_enable_chn", result); return -1; }
    ao_channel_enabled = 1;
    result = RK_MPI_AO_EnableReSmp(0, 0, AUDIO_SAMPLE_RATE_16000);
    if (result != RK_SUCCESS) { report_error("ao_enable_resmp", result); return -1; }
    result = RK_MPI_AO_SetVolume(0, (int32_t)current_ao_volume);
    if (result != RK_SUCCESS) { report_error("ao_set_volume", result); return -1; }
    result = RK_MPI_AO_SetTrackMode(0, AUDIO_TRACK_OUT_STEREO);
    if (result != RK_SUCCESS) { report_error("ao_set_track", result); return -1; }
    if (open_aac_decoder() != 0) { report_error("decoder_open", -1); return -1; }

    fprintf(stderr, "media_speaker_ready=aac+pcm/16000-to-8000/mono volume=%u/%u gpio=%d card=%s\n",
            current_ao_volume, MAX_AO_VOLUME, SPEAKER_GPIO, audio_card);
    return 0;
}

static void close_speaker(void) {
    speaker_gpio_cleanup();
    if (ao_channel_enabled) {
        RK_MPI_AO_ClearChnBuf(0, 0);
        RK_MPI_AO_DisableReSmp(0, 0);
        RK_MPI_AO_DisableChn(0, 0);
        ao_channel_enabled = 0;
    }
    ao_enabled = 0;
}

static int send_pcm_frame(const INT_PCM *samples, uint32_t sample_count,
                          uint32_t channels, uint64_t timestamp) {
    MB_EXT_CONFIG_S mb_config;
    AUDIO_FRAME_S frame;
    MB_BLK block = NULL;
    uint32_t pcm_bytes = sample_count * (uint32_t)sizeof(INT_PCM);
    int result;

    memset(&mb_config, 0, sizeof(mb_config));
    mb_config.pu8VirAddr = (uint8_t *)samples;
    mb_config.u64Size = pcm_bytes;
    mb_config.pOpaque = (void *)samples;
    result = RK_MPI_SYS_CreateMB(&block, &mb_config);
    if (result != RK_SUCCESS || !block) {
        report_error("create_pcm_mb", result);
        return -1;
    }

    memset(&frame, 0, sizeof(frame));
    frame.pMbBlk = block;
    frame.enBitWidth = AUDIO_BIT_WIDTH_16;
    frame.enSoundMode = channels == 1 ? AUDIO_SOUND_MODE_MONO
                                      : AUDIO_SOUND_MODE_STEREO;
    frame.u64TimeStamp = timestamp;
    frame.u32Seq = stream_sequence++;
    if (channels == 1) {
        frame.u32Len = pcm_bytes;
    } else if (channels == 2) {
        frame.u32Len = pcm_bytes >> 1;
    } else if (channels == 4) {
        frame.u32Len = pcm_bytes >> 2;
    } else {
        (void)RK_MPI_MB_ReleaseMB(block);
        report_error("unsupported_channels", (int)channels);
        return -1;
    }
    frame.bBypassMbBlk = 0;

    if (!speaker_gpio_active && speaker_gpio_set(1) != 0) {
        (void)RK_MPI_MB_ReleaseMB(block);
        report_error("speaker_gpio_enable", -1);
        return -1;
    }
    result = RK_MPI_AO_SendFrame(0, 0, &frame, 1000);
    if (result != RK_SUCCESS) {
        report_error("ao_send_frame", result);
        (void)RK_MPI_MB_ReleaseMB(block);
        return -1;
    }
    (void)RK_MPI_MB_ReleaseMB(block);
    return 0;
}

static int speaker_send_aac(unsigned char *data, uint32_t length,
                            uint64_t timestamp) {
    UCHAR *input = (UCHAR *)data;
    UINT bytes_valid = length;
    INT_PCM pcm[4096];
    unsigned int frames_sent = 0;

    if (!aac_decoder && open_aac_decoder() != 0) {
        report_error("decoder_open", -1);
        return -1;
    }
    for (unsigned int attempt = 0; attempt < 64; ++attempt) {
        AAC_DECODER_ERROR error;
        CStreamInfo *stream_info;
        uint32_t sample_count;

        if (bytes_valid > 0) {
            UINT buffer_size = bytes_valid;
            error = aacDecoder_Fill(aac_decoder, &input, &buffer_size, &bytes_valid);
            if (error != AAC_DEC_OK) { report_error("aac_fill", error); return -1; }
        }
        error = aacDecoder_DecodeFrame(aac_decoder, pcm,
                                       (UINT)(sizeof(pcm) / sizeof(pcm[0])), 0);
        if (error == AAC_DEC_NOT_ENOUGH_BITS) break;
        if (error != AAC_DEC_OK) { report_error("aac_decode", error); return -1; }

        stream_info = aacDecoder_GetStreamInfo(aac_decoder);
        if (!stream_info || stream_info->sampleRate == 0 ||
            stream_info->numChannels == 0 || stream_info->frameSize <= 0) {
            report_error("aac_stream_info", -1);
            return -1;
        }
        sample_count = (uint32_t)stream_info->frameSize * stream_info->numChannels;
        if (sample_count > (uint32_t)(sizeof(pcm) / sizeof(pcm[0]))) {
            report_error("aac_frame_size", (int)sample_count);
            return -1;
        }
        for (uint32_t sample = 0; sample < sample_count; ++sample) {
            pcm[sample] = (INT_PCM)(pcm[sample] >> PCM_ATTENUATION_SHIFT);
        }
        if (send_pcm_frame(pcm, sample_count, stream_info->numChannels,
                           timestamp) != 0) {
            return -1;
        }
        frames_sent++;
    }
    if (frames_sent == 0) return 1;
    return 0;
}

static int speaker_send_pcm16(unsigned char *data, uint32_t length,
                              uint64_t timestamp) {
    INT_PCM *samples;

    if (length == 0 || length % sizeof(INT_PCM) != 0) {
        report_error("pcm_length", (int)length);
        return -1;
    }
    if (length > MAX_PCM_PAYLOAD) {
        report_error("pcm_payload", (int)length);
        return -1;
    }
    samples = (INT_PCM *)data;
    for (uint32_t sample = 0; sample < length / sizeof(INT_PCM); ++sample) {
        samples[sample] = (INT_PCM)(samples[sample] >> PCM_ATTENUATION_SHIFT);
    }
    return send_pcm_frame(samples, length / (uint32_t)sizeof(INT_PCM), 1,
                          timestamp);
}

static void speaker_clear(void) {
    if (speaker_gpio_active) (void)speaker_gpio_set(0);
    RK_MPI_AO_ClearChnBuf(0, 0);
}

/* ------------------------------------------------------------------ */
/* Client command processing (reads speaker data from daemon)         */
/* ------------------------------------------------------------------ */

static uint32_t get_le32(const unsigned char *value) {
    return ((uint32_t)value[0]) | ((uint32_t)value[1] << 8) |
           ((uint32_t)value[2] << 16) | ((uint32_t)value[3] << 24);
}

static uint64_t get_le64(const unsigned char *value) {
    uint64_t result = 0;
    for (unsigned int index = 0; index < 8; ++index) {
        result |= (uint64_t)value[index] << (index * 8);
    }
    return result;
}

static int read_exact(int descriptor, unsigned char *buffer, size_t length) {
    while (length > 0) {
        ssize_t count = read(descriptor, buffer, length);
        if (count < 0 && errno == EINTR) {
            if (stopping) return 0;
            continue;
        }
        if (count == 0) return 0;
        if (count < 0) return -1;
        buffer += count;
        length -= (size_t)count;
    }
    return 1;
}

static int serve_client(int descriptor) {
    unsigned char header[FRAME_HEADER_LEN];
    unsigned char *payload = malloc(MAX_AAC_PAYLOAD);

    if (!payload) return -1;
    while (!stopping) {
        int status = read_exact(descriptor, header, sizeof(header));
        if (status <= 0) break;
        if (memcmp(header, "C220", 4) != 0) {
            fprintf(stderr, "media_client_bad_magic=1\n");
            break;
        }
        uint32_t length = get_le32(&header[4]);
        uint64_t timestamp = get_le64(&header[8]);
        unsigned int kind = header[17];

        if (kind == KIND_SPEAKER_CLEAR) {
            speaker_clear();
            continue;
        }
        if (kind == KIND_SPEAKER_VOLUME) {
            if (length != 4) continue;
            if (read_exact(descriptor, payload, 4) <= 0) break;
            uint32_t volume = get_le32(payload);
            if (volume <= MAX_AO_VOLUME &&
                RK_MPI_AO_SetVolume(0, (int32_t)volume) == RK_SUCCESS) {
                current_ao_volume = volume;
                fprintf(stderr, "media_speaker_volume=%u/%u\n", volume,
                        MAX_AO_VOLUME);
            }
            continue;
        }
        if (kind == KIND_NIGHT_MODE) {
            if (length < 1) continue;
            if (read_exact(descriptor, payload, length) <= 0) break;
            int mode = payload[0];
            if (mode == 0) {
                pthread_mutex_lock(&night_lock);
                night_mode_target = -1;
                pthread_mutex_unlock(&night_lock);
                fprintf(stderr, "media_night_mode_set=auto\n");
            } else if (mode == 1) {
                pthread_mutex_lock(&night_lock);
                night_mode_target = 1;
                pthread_mutex_unlock(&night_lock);
                apply_night_mode(1);
                fprintf(stderr, "media_night_mode_set=night\n");
            } else if (mode == 2) {
                pthread_mutex_lock(&night_lock);
                night_mode_target = 0;
                pthread_mutex_unlock(&night_lock);
                apply_night_mode(0);
                fprintf(stderr, "media_night_mode_set=day\n");
            }
            continue;
        }
        if (kind != KIND_SPEAKER_PCM) {
            if (length > MAX_AAC_PAYLOAD) break;
            if (length > 0 && read_exact(descriptor, payload, length) <= 0) break;
            continue;
        }
        if (length == 0 || length > MAX_AAC_PAYLOAD) break;
        if (read_exact(descriptor, payload, length) <= 0) break;
        if (header[16] == 1) {
            (void)speaker_send_pcm16(payload, length, timestamp);
        } else {
            (void)speaker_send_aac(payload, length, timestamp);
        }
    }
    free(payload);
    return 0;
}

static void *audio_out_thread(void *argument) {
    (void)argument;
    if (init_speaker() != 0) return NULL;
    while (!stopping) {
        struct sockaddr_un address;
        socklen_t address_length = sizeof(address);
        int descriptor;

        if (listen_fd < 0 || stopping) break;
        descriptor = accept(listen_fd, (struct sockaddr *)&address,
                            &address_length);
        if (descriptor < 0) {
            if (errno == EINTR) continue;
            if (stopping) break;
            usleep(10000);
            continue;
        }
        fprintf(stderr, "media_client_connected=1\n");
        client_replace(descriptor);
        (void)serve_client(descriptor);
        {
            int active = client_current();
            if (active == descriptor) client_replace(-1);
            else close(descriptor);
        }
        fprintf(stderr, "media_client_disconnected=1\n");
    }
    close_speaker();
    return NULL;
}

/* ------------------------------------------------------------------ */
/* Listener setup and main                                             */
/* ------------------------------------------------------------------ */

static int start_listener(void) {
    struct sockaddr_un address;

    unlink(socket_path);
    listen_fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (listen_fd < 0) {
        report_error("socket", -1);
        return -1;
    }
    memset(&address, 0, sizeof(address));
    address.sun_family = AF_UNIX;
    snprintf(address.sun_path, sizeof(address.sun_path), "%s", socket_path);
    if (bind(listen_fd, (struct sockaddr *)&address, sizeof(address)) < 0) {
        report_error("bind", -1);
        return -1;
    }
    chmod(socket_path, 0600);
    if (listen(listen_fd, 2) < 0) {
        report_error("listen", -1);
        return -1;
    }
    fprintf(stderr, "media_socket_ready=%s\n", socket_path);
    return 0;
}

int main(int argc, char **argv) {
    struct sigaction action;
    pthread_t video, audio, speaker;
    pthread_attr_t thread_attr;

    for (int index = 1; index < argc; ++index) {
        if (strcmp(argv[index], "--socket") == 0 && index + 1 < argc) socket_path = argv[++index];
        else if (strcmp(argv[index], "--video-device") == 0 && index + 1 < argc) video_device = argv[++index];
        else if (strcmp(argv[index], "--card") == 0 && index + 1 < argc) audio_card = argv[++index];
        else if (strcmp(argv[index], "--iq-dir") == 0 && index + 1 < argc) iq_dir = argv[++index];
        else if (strcmp(argv[index], "--sub") == 0) sub_enabled = 1;
        else if (strcmp(argv[index], "--sub-dry") == 0) {
            /* Diagnose the cost split: scale only, never feed the encoder. */
            sub_enabled = 1;
            sub_dry = 1;
        } else if (strcmp(argv[index], "--no-sub") == 0) sub_enabled = 0;
        else if (strcmp(argv[index], "--sub-fps") == 0) {
            sub_fps = atoi(argv[++index]);
            if (sub_fps < 1) sub_fps = 1;
        } else if (strcmp(argv[index], "--sub-bitrate") == 0) {
            sub_bitrate_kbps = atoi(argv[++index]);
        } else if (strcmp(argv[index], "--fps") == 0) {
            video_fps = atoi(argv[++index]);
        } else if (strcmp(argv[index], "--bitrate") == 0) {
            video_bitrate_kbps = atoi(argv[++index]);
        }
    }

    memset(&action, 0, sizeof(action));
    action.sa_handler = on_signal;
    sigemptyset(&action.sa_mask);
    sigaction(SIGINT, &action, NULL);
    sigaction(SIGTERM, &action, NULL);
    signal(SIGPIPE, SIG_IGN);

    setenv("rt_log_level", "0", 1);
    setenv("rt_log_path", "/tmp/c220-media-rockit.log", 1);
    int null_fd = open("/dev/null", O_WRONLY);
    if (null_fd >= 0) {
        dup2(null_fd, STDOUT_FILENO);
        close(null_fd);
    }
    setvbuf(stdout, NULL, _IONBF, 0);

    if (RK_MPI_SYS_Init() != RK_SUCCESS) {
        report_error("sys_init", -1);
        _exit(2);
    }
    sys_initialized = 1;

    if (start_listener() != 0) _exit(2);

    if (RK_MPI_SYS_MmzAlloc(&video_input, NULL, NULL, (RK_U32)FRAME_SIZE) != RK_SUCCESS ||
        !video_input) {
        report_error("video_mmz", -1);
        _exit(2);
    }
    video_input_ptr = (unsigned char *)RK_MPI_MB_Handle2VirAddr(video_input);
    if (!video_input_ptr) {
        report_error("video_mmz_viraddr", -1);
        _exit(2);
    }

    pthread_attr_init(&thread_attr);
    pthread_attr_setstacksize(&thread_attr, 1024 * 1024);

    if (pthread_create(&video, &thread_attr, video_thread, NULL) != 0) {
        report_error("video_thread", -1);
        _exit(2);
    }
    if (pthread_create(&audio, &thread_attr, audio_thread, NULL) != 0) {
        report_error("audio_thread", -1);
        _exit(2);
    }
    if (pthread_create(&speaker, &thread_attr, audio_out_thread, NULL) != 0) {
        report_error("speaker_thread", -1);
        _exit(2);
    }
    if (pthread_create(&night_thread, &thread_attr, night_monitor_thread, NULL) == 0) {
        night_thread_started = 1;
    }
    pthread_attr_destroy(&thread_attr);

    fprintf(stderr, "media_engine_ready=1 socket=%s device=%s codec=h265\n", socket_path,
            video_device);

    while (!stopping) sleep(1);

    if (listen_fd >= 0) {
        shutdown(listen_fd, SHUT_RDWR);
        close(listen_fd);
        listen_fd = -1;
    }
    if (night_thread_started) {
        pthread_join(night_thread, NULL);
        night_thread_started = 0;
    }
    pthread_join(speaker, NULL);
    pthread_join(audio, NULL);
    pthread_join(video, NULL);

    if (video_input) {
        RK_MPI_SYS_MmzFree(video_input);
        video_input = NULL;
    }
    if (sys_initialized) {
        sys_initialized = 0;
    }
    unlink(socket_path);
    fprintf(stderr, "media_engine_stopped=1\n");
    return 0;
}
