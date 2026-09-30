#include "PcMonitorService.h"

#include <HardwareSerial.h>

#include "ClaudeService.h"
#include "JsonSerializer.h"

void PcMonitorService::recordCpuSample(PcStats& pc) {
    float value = isnan(pc.cpu_percent) ? 0 : constrain(pc.cpu_percent, 0, 100);
    pc.cpu_history[pc.history_head] = (uint8_t)round(value);
    pc.history_head = (pc.history_head + 1) % CPU_HISTORY_LEN;
    if (pc.history_count < CPU_HISTORY_LEN) pc.history_count++;
    pc.last_sample = millis();
}

// Shared by the USB (serial) and Wi-Fi (/pc-stats) telemetry paths.
void PcMonitorService::applyMediaTelemetry(const JsonDocument& doc, PcMedia& media) {
    media.status = doc["media_status"] | "stopped";
    media.name = doc["media_name"] | "";
    media.author = doc["media_author"] | "";
    media.album = doc["media_album"] | "";

    // Older PC app versions send no position; the progress bar is then hidden.
    unsigned long duration = doc["media_len"] | 0UL;
    unsigned long position = doc["media_pos"] | 0UL;
    media.duration_sec = duration;
    media.position_sec = duration > 0 ? min(position, duration) : 0;
    media.position_at = millis();
}

// Advances the last reported position while playing, so the bar moves every
// redraw even though the PC only reports every few seconds.
unsigned long PcMonitorService::currentPosition(const PcMedia& media) {
    unsigned long pos = media.position_sec;
    if (media.status.equalsIgnoreCase("playing")) {
        pos += (millis() - media.position_at) / 1000;
    }
    return media.duration_sec > 0 ? min(pos, media.duration_sec) : pos;
}

void PcMonitorService::setBrightnessCallback(SerialBrightnessCallback callback) {
    brightnessCallback = callback;
}

// "SET_BRIGHTNESS:<1-100>:<0|1>" - the trailing flag persists the value.
void PcMonitorService::handleBrightnessCommand(const String& args) {
    int sep = args.indexOf(':');
    String valueStr = sep == -1 ? args : args.substring(0, sep);
    bool persist = sep != -1 && args.substring(sep + 1) == "1";

    int value = valueStr.toInt();
    if (valueStr.length() == 0 || valueStr.length() > 3 || value < 1 || value > 100) return;
    if (brightnessCallback) brightnessCallback(value, persist);
}

bool PcMonitorService::handleSerial(AppState &state) {
    bool configUpdated = false;

    while (Serial.available()) {
        String incoming = Serial.readStringUntil('\n');
        incoming.trim();
        
        if (incoming.length() > 0) {
            if (incoming == "GET_UPDATE") {
                String json = JsonSerializer::buildAppStateJson(state);
                Serial.print("SYS_UPDATE:");
                Serial.println(json);
            } 
            else if (incoming.startsWith("SET_BRIGHTNESS:")) {
                handleBrightnessCommand(incoming.substring(15));
            }
            else if (incoming.startsWith("SAVE_CFG:")) {
                if (JsonSerializer::parseConfig(incoming.substring(9).c_str(), state)) {
                    configUpdated = true;
                }
            } 
            else if (incoming.startsWith("{")) {
                parseJson(incoming.c_str(), state);
            }
        }
    }

    unsigned long activeTimeout = state.pc.is_wifi ? WIFI_DATA_TIMEOUT_MS : DATA_TIMEOUT_MS;

    // Sample on a fixed clock (not per packet) so USB and Wi-Fi graphs share one time scale.
    bool pcDataFresh = state.pc.last_update != 0 && millis() - state.pc.last_update <= activeTimeout;
    if (pcDataFresh && millis() - state.pc.last_sample >= CPU_SAMPLE_MS) {
        recordCpuSample(state.pc);
    }

    if (millis() - state.pc.last_update > activeTimeout) {
        state.pc.cpu_percent = 0;
        state.pc.net_down_kb = 0;
        state.pc.net_up_kb = 0;
        state.pc.mem_percent = 0;
        state.pc.disk_percent = 0;
    }

    if (millis() - state.media.last_update > activeTimeout) {
        state.media.status = "stopped";
        state.media.name = "";
        state.media.author = "";
        state.media.album = "";
        state.media.duration_sec = 0;
    }

    ClaudeService::expireIfStale(state.claude, activeTimeout);

    return configUpdated;
}

void PcMonitorService::parseJson(const char* jsonString, AppState &state) {
    DynamicJsonDocument doc(TELEMETRY_DOC_SIZE);
    DeserializationError error = deserializeJson(doc, jsonString);

    if (!error) {
        state.pc.cpu_percent = doc["cpu_percent"] | 0.0;
        state.pc.mem_percent = doc["mem_percent"] | 0.0;
        state.pc.disk_percent = doc["disk_percent"] | 0.0;
        state.pc.net_down_kb = doc["net_down_kb"] | 0.0;
        state.pc.net_up_kb = doc["net_up_kb"] | 0.0;
        
        applyMediaTelemetry(doc, state.media);

        ClaudeService::applyTelemetry(doc, state.claude);
        
        String incoming_id = doc["pc_id"] | "";
        if (incoming_id != "") {
            state.config.active_pc_id = incoming_id;
        }
        
        unsigned long current_time = millis();
        state.pc.last_update = current_time; 
        state.pc.is_wifi = false;
        state.media.last_update = current_time;
    }
}