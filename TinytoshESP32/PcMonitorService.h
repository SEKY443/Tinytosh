#ifndef PC_MONITOR_SERVICE_H
#define PC_MONITOR_SERVICE_H

#include <Arduino.h>
#include <ArduinoJson.h>
#include "structs.h"

typedef void (*SerialBrightnessCallback)(int percent, bool persist);

class PcMonitorService {
public:
    bool handleSerial(AppState &state);
    void setBrightnessCallback(SerialBrightnessCallback callback);
    static void applyMediaTelemetry(const JsonDocument& doc, PcMedia& media);
    static unsigned long currentPosition(const PcMedia& media);

private:
    static const int JSON_BUF_SIZE = 256;
    static const int TELEMETRY_DOC_SIZE = 2048;
    static const unsigned long DATA_TIMEOUT_MS = 3000;
    static const unsigned long WIFI_DATA_TIMEOUT_MS = 10000;
    static const unsigned long CPU_SAMPLE_MS = 1000;

    static void recordCpuSample(PcStats& pc);

    PcStats currentStats = {0.0, 0.0, 0.0, 0.0};
    char serialBuffer[JSON_BUF_SIZE];
    int bufferIndex = 0;
    SerialBrightnessCallback brightnessCallback = nullptr;

    void handleBrightnessCommand(const String& args);

    static void parseJson(const char* jsonString, AppState &state);
};

#endif