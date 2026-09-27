#include "ClaudeService.h"

void ClaudeService::applyTelemetry(const JsonDocument& doc, ClaudeData& data) {
    // Older bridge versions do not send Claude fields; leave the data to expire.
    if (!doc.containsKey("claude_state")) return;

    String state = doc["claude_state"] | "";
    data.state = isKnownState(state) ? state : "idle";
    data.tool = sanitizeLabel(doc["claude_tool"] | "");
    data.project = sanitizeLabel(doc["claude_proj"] | "");
    data.busy_sessions = constrain((int)(doc["claude_sessions"] | 0), 0, 99);

    data.usage_ok = doc["claude_ok"] | false;
    data.five_hour_pct = constrain((int)(doc["claude_5h"] | 0), 0, 100);
    data.five_hour_reset_min = max((int)(doc["claude_5h_reset"] | 0), 0);
    data.weekly_pct = constrain((int)(doc["claude_7d"] | 0), 0, 100);
    data.weekly_reset_min = max((int)(doc["claude_7d_reset"] | 0), 0);

    data.last_update = millis();
}

void ClaudeService::expireIfStale(ClaudeData& data, unsigned long timeoutMs) {
    if (data.state.length() > 0 && millis() - data.last_update > timeoutMs) {
        clear(data);
    }
}

void ClaudeService::clear(ClaudeData& data) {
    data = ClaudeData();
}

bool ClaudeService::hasData(const ClaudeData& data) {
    if (data.state.length() == 0) return false;
    return data.state != "offline" || data.usage_ok;
}

bool ClaudeService::needsUser(const ClaudeData& data) {
    return data.state == "permission";
}

String ClaudeService::formatDuration(int minutes) {
    if (minutes <= 0) return "NOW";
    if (minutes < 60) return String(minutes) + "M";
    if (minutes < 24 * 60) return String(minutes / 60) + "H " + String(minutes % 60) + "M";
    return String(minutes / (24 * 60)) + "D " + String((minutes / 60) % 24) + "H";
}

bool ClaudeService::isKnownState(const String& state) {
    return state == "offline" || state == "idle" || state == "thinking" || state == "tool" ||
           state == "writing" || state == "permission" || state == "limit";
}

// The OLED font only covers printable ASCII; drop everything else.
String ClaudeService::sanitizeLabel(const char* raw) {
    String out;
    for (int i = 0; raw[i] != '\0' && out.length() < MAX_LABEL_LEN; i++) {
        char c = raw[i];
        if (c >= 0x20 && c <= 0x7E) out += c;
    }
    return out;
}
