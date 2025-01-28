.PHONY: all clean libltntstools ffmpeg libltntstools_clean ffmpeg_clean submodules setup install build container

# Check if cmake3 exists and use it; otherwise, use cmake
JOBS := $(shell nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 8)

all: ffmpeg libltntstools build

install:
	mkdir -p bin && cp -f target/release/udp-to-hls hls-to-udp/target/release/hls-to-udp bin/

setup:
	sh scripts/setup_system.sh

build:
	cargo build --release
	cd hls-to-udp && cargo build --release

submodules:
	git submodule update --init --recursive

clean: ffmpeg_clean libltntstools_clean
	cargo clean
	cd hls-to-udp && cargo clean
	rm -rf bin

container: clean
	podman-compose build

ffmpeg:
	cd src/include/FFmpeg && \
		./configure --prefix=`pwd`/target-root \
		--disable-iconv --enable-static --disable-shared --disable-debug \
		--disable-programs --disable-doc --enable-small \
		--enable-decoder=aac,ac3,eac3,mp2,mp3,h264,hevc,mpeg2video,mjpeg \
		--enable-encoder=mjpeg \
		--enable-demuxer=mpegts,image2 \
		--enable-muxer=mpegts,image2 \
		--enable-parser=aac,ac3,eac3,mp2,mp3,h264,hevc,mpeg2video \
		--enable-protocol=udp,file \
		--enable-pthreads --enable-gpl \
		--disable-audiotoolbox --disable-videotoolbox --disable-avfoundation \
		--enable-optimizations --extra-cflags="-O3 -fPIC" --extra-cxxflags="-O3 -fPIC" && \
			make -j $(JOBS) && make install

ffmpeg_debug:
	cd src/include/FFmpeg && \
		./configure --prefix=`pwd`/target-root \
		--disable-programs --disable-doc \
		--enable-decoder=aac,ac3,eac3,mp2,mp3,h264,hevc,mpeg2video,mjpeg \
		--enable-encoder=mjpeg \
		--enable-demuxer=mpegts,image2 \
		--enable-muxer=mpegts,image2 \
		--enable-parser=aac,ac3,eac3,mp2,mp3,h264,hevc,mpeg2video \
		--enable-protocol=udp,file \
		--enable-pthreads --enable-gpl \
		--disable-iconv --enable-static --disable-shared --enable-debug \
		--disable-audiotoolbox --disable-videotoolbox --disable-avfoundation \
		--extra-cflags="-g -fPIC" --extra-cxxflags="-g -fPIC" && \
			make -j $(JOBS) && make install

ffmpeg_clean:
	cd src/include/FFmpeg && make clean 2>/dev/null || echo ""
	rm -rf src/include/FFmpeg/target-root

libltntstools_clean:
	cd src/include/libltntstools && make clean 2>/dev/null || echo ""
	rm -rf src/include/libltntstools/target-root

libltntstools:
	cd src/include/libltntstools && \
		./autogen.sh --build && \
		CPPFLAGS="-O3 -I../../FFmpeg/target-root/include/ -I../../FFmpeg" \
		CFLAGS="-O3 -I../../FFmpeg/target-root/include/ -I../../FFmpeg" \
		LDFLAGS="-L../../FFmpeg/target-root/lib" \
		./configure --prefix=`pwd`/target-root --enable-shared=no --enable-static && \
				make -j $(JOBS) && make install

libltntstools_debug:
	cd src/include/libltntstools && \
		./autogen.sh --build && \
		CPPFLAGS="-g -I../../FFmpeg/target-root/include/ -I../../FFmpeg" \
		CFLAGS="-g -I../../FFmpeg/target-root/include/ -I../../FFmpeg" \
		LDFLAGS="-L../../FFmpeg/target-root/lib" \
		./configure --enable-debug --prefix=`pwd`/target-root --enable-shared=no --enable-static && \
				make -j $(JOBS) && make install

