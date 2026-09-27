#ifndef CLAUDE_SERVICE_H
#define CLAUDE_SERVICE_H

#include <Arduino.h>
#include <ArduinoJson.h>
#include "structs.h"

class ClaudeService {
public:
    static void applyTelemetry(const JsonDocument& doc, ClaudeData& data);
    static void expireIfStale(ClaudeData& data, unsigned long timeoutMs);
    static void clear(ClaudeData& data);

    static bool hasData(const ClaudeData& data);
    static bool needsUser(const ClaudeData& data);
    static String formatDuration(int minutes);

private:
    static const int MAX_LABEL_LEN = 20;

    static bool isKnownState(const String& state);
    static String sanitizeLabel(const char* raw);
};

#endif
