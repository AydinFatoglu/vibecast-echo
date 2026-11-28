# Show8Cast — 32-bit Cast Receiver for Echo Show 8 (1st Gen)

**Show8Cast** is a VibeCast fork adapted for the first-generation Amazon Echo
Show 8. It provides a dedicated **32-bit ARM (`armeabi-v7a`) Android receiver**
for devices that cannot install the 64-bit APK published by the upstream
VibeCast project.

The app turns an Echo Show 8 (1st Gen) into a local Cast target. A phone on the
same Wi-Fi network can discover **Show8Cast** and send YouTube playback to the
device. Tapping the screen reveals play/pause, next, and stop controls; an idle
image is displayed when no video is playing.

## Why this fork exists

The first-generation Echo Show 8 runs a 32-bit Android userspace and reports
`armeabi-v7a, armeabi`. It cannot install an APK that contains only 64-bit
`arm64-v8a` native libraries.

The upstream [VibeCast](https://github.com/emilsvennesson/vibecast) source can
target multiple architectures, but its published APK is not installable on this
32-bit Echo Show. This fork adds the Echo-specific receiver experience and a
documented ARMv7 build path:

```sh
cd android
./gradlew :app:assembleDebug -Pvibecast.abis=armeabi-v7a
```

The resulting APK contains `lib/armeabi-v7a/libvibecast_ffi.so` and can be
installed on the Echo Show 8 (1st Gen).

## Features

- Native 32-bit ARM (`armeabi-v7a`) build for the Echo Show 8 (1st Gen)
- Local Cast discovery on the same Wi-Fi network
- YouTube discovery and playback through DIAL
- On-screen playback controls designed for the Echo Show touchscreen
- Automatic receiver startup after boot
- No separate analytics or advertising SDK

## Requirements

- Amazon Echo Show 8 (1st Gen), or another Android 7.1.2+ ARMv7 device
- ADB access for installation
- Phone and receiver connected to the same local network

## Install

Build the ARMv7 APK using the command above, then install it with ADB:

```sh
adb install -r android/app/build/outputs/apk/debug/Show8Cast-debug.apk
```

Open **Show8Cast** on the Echo Show. On your phone, open YouTube, choose the
Cast button, select **Show8Cast**, and start playback.

For complete build, certificate, and diagnostic instructions, see the
[Android guide](android/README.md).

## Upstream and license

This project is based on
[emilsvennesson/vibecast](https://github.com/emilsvennesson/vibecast) and is
adapted for the 32-bit Echo Show 8 (1st Gen). It is distributed under the
[MIT License](LICENSE).
