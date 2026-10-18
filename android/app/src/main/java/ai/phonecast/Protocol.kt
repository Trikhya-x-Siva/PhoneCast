package ai.phonecast

/** Wire protocol shared with the PC app. See PROTOCOL.md. */
object Protocol {
    const val PORT = 27183
    const val VERSION = 1

    // phone -> PC
    const val VIDEO_CONFIG = 0x01
    const val VIDEO_FRAME = 0x02
    const val AUDIO_CONFIG = 0x03
    const val AUDIO_PCM = 0x04
    const val STATS = 0x05
    const val PONG = 0x06
    const val MIC_PCM = 0x07
    const val FOREGROUND_APP = 0x08

    // PC -> phone
    const val SET_BITRATE = 0x81
    const val REQUEST_KEYFRAME = 0x82
    const val PING = 0x83
    const val STOP = 0x84
    const val SET_MIC = 0x85
    const val SET_PRIVACY = 0x86

    const val FLAG_KEYFRAME = 0x01

    const val CODEC_H264 = 1
    const val CODEC_H265 = 2

    const val AUDIO_S16LE = 1
}

class Packet(
    val type: Int,
    val flags: Int,
    val ptsUs: Long,
    val payload: ByteArray,
) {
    val isVideoFrame get() = type == Protocol.VIDEO_FRAME
    val isKeyframe get() = (flags and Protocol.FLAG_KEYFRAME) != 0
}
