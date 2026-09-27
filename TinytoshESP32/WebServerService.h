#ifndef WEB_SERVER_SERVICE_H
#define WEB_SERVER_SERVICE_H

#include <WebServer.h>
#include <WiFiManager.h>
#include "structs.h"

typedef void (*ConfigSaveCallback)();
typedef void (*BrightnessCallback)(int percent, bool persist);

class WebServerService {
public:
    WebServerService(int port, ConfigSaveCallback callback);
    void begin();
    void handleClient();
    void setAppState(AppState* appState);
    void setBrightnessCallback(BrightnessCallback callback);
    
    void handleRoot();
    void handleSave();
    void handleUpdate();
    void handlePcStats();
    void handleBrightness();
    
private:
    static const unsigned long PC_DATA_TIMEOUT_MS = 10000;
    static const int TELEMETRY_DOC_SIZE = 2048;
    static const int HTTP_OK = 200;
    static const int HTTP_REDIRECT = 302;
    static const int HTTP_BAD_REQUEST = 400;
    static const int HTTP_FORBIDDEN = 403;
    static constexpr const char* LOCAL_DOMAIN_NAME = "tinytosh";

    WebServer server;
    WiFiManager wm;
    ConfigSaveCallback saveCallback;
    BrightnessCallback brightnessCallback = nullptr;
    
    AppState* state;
};

#endif