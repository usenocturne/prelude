# Only the libraries gstreamer1.0-libav links against, built LGPL with just the
# Dolby Digital (AC-3) and Dolby Digital Plus (E-AC-3) decoders that AirPlay
# video streams need and the Car Thing has no other decoder for.
PACKAGECONFIG = "avcodec avformat avfilter swresample"
EXTRA_FFCONF = "--disable-everything --disable-programs --disable-doc \
    --enable-decoder=ac3,ac3_fixed,eac3 --enable-parser=ac3"
