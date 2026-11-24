/*
 * Minimal AIDL mirror of android.hardware.light (V2, Android 14+).
 * Field types are plain ints so the wire format is byte-identical to the
 * upstream enums, but rsbinder does not need the enum definitions.
 */
package android.hardware.light;

@VintfStability
parcelable HwLightState {
    int color;
    int flashMode;
    int flashOnMs;
    int flashOffMs;
    int brightnessMode;
}
