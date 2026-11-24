package android.hardware.light;

import android.hardware.light.HwLightState;
import android.hardware.light.HwLight;

@VintfStability
interface ILights {
    void setLightState(int id, in HwLightState state);
    HwLight[] getLights();
}
