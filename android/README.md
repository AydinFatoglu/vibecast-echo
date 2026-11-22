# vibecast for Android TV

An Android TV app that runs the vibecast receiver on the device. The Rust core
is loaded through UniFFI-generated Kotlin bindings and runs in a foreground
service, advertising each connected player over `NsdManager`. It runs alongside
the device's built-in Cast receiver.

The Show8Cast frontend includes a native Media3 player that connects to the
player bridge on port `8010`. YouTube starts automatically and advertises the
screen over DIAL so phones on the same network can discover it without entering
a TV code. External Kodi and browser players can also connect to the bridge.

## Requirements

- Android 7.1.2 (API 25) or later; `arm64-v8a`, `armeabi-v7a` or `x86_64`
- Device-auth certificates are optional for YouTube's DIAL flow; CastV2
  authentication still needs them (see [Certificates](https://github.com/emilsvennesson/vibecast/blob/main/README.md#certificates)).

## Build

Prerequisites:

- Android SDK with platform 37 and build-tools 36
- Android NDK r28+ (`ANDROID_NDK_HOME`, or the newest under `$ANDROID_HOME/ndk/`)
- JDK 17
- Rust with the Android targets and `cargo-ndk`:
  ```sh
  rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
  cargo install cargo-ndk
  ```

```sh
cd android
./gradlew :app:assembleDebug                                # APK (all ABIs)
./gradlew :app:assembleDebug lintDebug ktlintCheck detekt   # APK + lint
```

To cut build time and APK size for one device family, narrow the ABI set with
`-Pvibecast.abis`. The Amazon Echo Show 8 Gen1 (Android 7.1.2 / API 25) reports
`armeabi-v7a,armeabi` and has no 64-bit app support, so it only needs:

```sh
./gradlew :app:assembleDebug -Pvibecast.abis=armeabi-v7a
```

## Install and provision certificates

`certs.json` is never bundled in the APK. Copy it into the app's private
storage over adb (requires a debug build):

```sh
adb install -r app/build/outputs/apk/debug/Show8Cast-debug.apk

PKG=com.vibecast.receiver
adb shell run-as "$PKG" mkdir -p files
adb push ~/.vibecast/certs.json /data/local/tmp/certs.json
adb shell run-as "$PKG" cp /data/local/tmp/certs.json files/certs.json
adb shell rm /data/local/tmp/certs.json
```

## Run

Open Show8Cast on the TV, or:

```sh
adb shell am start -n com.vibecast.receiver/.MainActivity
```

Then point a player at `<tv-ip>:8010`. Once it connects, it appears as a Cast
device on the network. Logs:

```sh
adb logcat -s vibecast
```
