# miui-lights

Backlight bridge + AIDL lights HAL untuk port MIUI/HyperOS (MT6789, Infinix X6886).

Latar belakang singkat: di port ini framework memilih
`BacklightAdapter [useSurfaceControl=false]`, tapi `LightImpl` tidak pernah
dipanggil (`mColor=0`) dan SurfaceFlinger menolak brightness
(`displayBrightness=nullopt`). Akibatnya `/sys/class/leds/lcd-backlight/brightness`
tidak pernah ditulis walaupun panelnya sendiri bisa ditulis manual.

Hasil probe di device membuktikan lights HAL bawaan **berfungsi** kalau dipanggil
langsung (`setLightState(0, color)` menulis sysfs dengan benar). Masalahnya cuma
tidak ada yang memanggil. Karena itu solusinya dua lapis:

1. **`--sync` (default)** — daemon event-driven berbasis Bionic futex
   (`__system_property_wait`) yang memantau `debug.tracing.screen_brightness`
   dan `debug.tracing.screen_state` langsung tanpa `logcat` maupun polling.
   Saat layar mati (`screen_state` 0/1) backlight dimatikan (`0`) dan update
   ditahan; saat layar menyala lagi (`screen_state` 2) backlight dipaksa
   tulis ulang lewat 2 tahap (*state-change level* -> target) agar driver
   `leds-mtk` tidak melewati nilai yang sama. Saat masuk AOD/DOZE (`1`/`3`/`4`)
   backlight diturunkan ke `min_hw_brightness`, dan update brightness ditahan
   sampai layar menyala kembali.
2. **`--hal`** — implementasi ulang `android.hardware.light.ILights/default`
   (AIDL V2) yang menulis sysfs. Dipakai kalau mau mengganti
   `vendor.light-default` bawaan di image.

## Build

Butuh Rust + NDK r27c (path linker di `.cargo/config.toml`).

```sh
cargo build --target aarch64-linux-android --release
# hasil: target/aarch64-linux-android/release/miui_lights
```

## Pakai di device

```sh
adb push miui_lights /data/local/tmp/miui_lights
adb shell chmod 755 /data/local/tmp/miui_lights

# WAJIB: hentikan daemon brightness bawaan ROM. Tanpa ini dua proses
# rebutan menulis node yang sama dan brightness bisa loncat sendiri.
adb shell 'stop vendor.xiaomi.hardware.milights-service; stop vendor.light-default'

# 1) bridge brightness (yang dipakai sehari-hari)
adb shell 'nohup /data/local/tmp/miui_lights --sync >/data/local/tmp/miui_lights.log 2>&1 </dev/null &'

# 2) tes tulis langsung
adb shell '/data/local/tmp/miui_lights --value 0.5'

# 3) probe lights HAL yang sedang jalan
adb shell '/data/local/tmp/miui_lights --probe'

# 4) ganti lights HAL (stop dulu HAL bawaan)
adb shell stop vendor.light-default
adb shell 'nohup /data/local/tmp/miui_lights --hal >/data/local/tmp/miui_lights_hal.log 2>&1 </dev/null &'
```

Opsi:

| opsi | arti | default |
|---|---|---|
| `--sync` | ikuti `debug.tracing.screen_brightness` + `screen_state` (futex) → sysfs | mode default |
| `--hal` | register `android.hardware.light.ILights/default` | – |
| `--probe` | baca `getLights()` + tes `setLightState` | – |
| `--value F` | tulis satu nilai 0.0–1.0 lalu keluar | – |
| `--gamma G` | gamma kurva brightness | 1.0 (linear) |
| `--gain K` | pengali brightness framework (`1/hbmMax` ≈ 2.0) | 2.0 |
| `--floor F` | batas minimum fraksi range raw (hanya saat > 0) | 0.01 |

Kurva default linear. Framework sudah menerapkan kurva perseptualnya sendiri,
jadi gamma tambahan membuat separuh bawah gelap dan separuh atas loncat terang.
Dengan `gain=2`, slider 25/50/75/100% memetakan ke ~25/50/75/100% daya panel.
Kalau ujung bawah masih terasa gelap, turunkan `--gamma` (mis. `0.85`). Kalau
terlalu terang di minimum, naikkan `--gamma`. Kalau brightness maksimum tidak
sampai terang penuh: naikkan `--gain` (mis. 2.5).

## Kenapa perlu bridge, bukan cukup hook framework?

Yang sudah dicoba dan hasilnya:

- hook `gui::BpSurfaceComposer::setDisplayBrightness` di system_server →
  `set_calls=0`, framework tidak pernah memanggilnya.
- slider ROM **tidak** menulis `screen_brightness_float` maupun langsung ke
  HAL. Framework menulis `debug.tracing.screen_brightness` dan
  `debug.tracing.screen_state`, sehingga bridge memakai kernel futex
  (`__system_property_wait`) pada dua property tersebut — tidak bergantung pada
  `logd` atau polling `cmd`.
- `BacklightAdapter` (`useSurfaceControl=false`) → `LightImpl.mColor` tetap 0,
  jadi bukan lewat lights HAL juga.
- panggil lights HAL langsung (`setLightState(0, 0xC8xxxxxx)`) → sysfs berubah.
  Jadi HAL sehat, framework-nya yang tidak menyambung.

Karena titik putusnya di framework (yang tidak bisa dipatch permanen tanpa
modul/Xposed), bridge sysfs adalah jalan paling stabil.

## Auto brightness

`--sync` ikut nilai `debug.tracing.screen_brightness`. Kalau auto-brightness framework
memang memperbarui nilai itu, bridge otomatis mengikuti. Di device ini auto
masih mati di level sensor: `mAmbientLux=-1.0` / `0.0 (old/invalid)` dan
`mAppliedAutoBrightness=false`, karena driver ALS `mn79911` tidak ada di kernel
port (tidak ada simbol di `/proc/kallsyms`, `odm:fakesensor` tidak ter-bind).
Itu butuh perbaikan kernel/driver, bukan userspace.

## Permanen (image rebuild)

`/vendor` erofs read-only, jadi untuk permanen:

1. taruh binary di `/vendor/bin/miui_lights` (atau `/system_ext/bin`).
2. pasang `init/miui-lights.rc` ke `/vendor/etc/init/`.
3. kalau memakai `--hal`, nonaktifkan service `vendor.light-default`
   (`/vendor/etc/init/lights-mtk-default.rc`) supaya tidak rebutan nama service.

Contoh rc ada di `init/miui-lights.rc`.
