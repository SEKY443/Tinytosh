#include <WiFi.h>
#include <WiFiManager.h>

#include "AirQualityService.h"
#include "BambuService.h"
#include "CalendarService.h"
#include "ClaudeService.h"
#include "ConfigManager.h"
#include "CryptoService.h"
#include "CurrencyService.h"
#include "DataSyncService.h"
#include "DaylightService.h"
#include "DisplayService.h"
#include "HardwareService.h"
#include "images.h"
#include "MoonService.h"
#include "NightModeService.h"
#include "PcMonitorService.h"
#include "PopulationService.h"
#include "StockService.h"
#include "structs.h"
#include "TimeService.h"
#include "WeatherService.h"
#include "WebServerService.h"

// Global Constants
const char* AP_SSID = "Tinytosh";
const char* AP_PASS = "Tinytosh";
const char* PREF_NAMESPACE = "tinytosh_config";

// Global Data Structure
AppState appState;

// Forward declarations of callbacks
void updateAllDataCallback();
void handleSingleClick();
void handleDoubleClick();
void handleLongPress();
void applyBrightness(int percent, bool persist);

// Service Instances
ConfigManager configManager(PREF_NAMESPACE);
DisplayService displayService(128, 64, -1);
WebServerService webServerService(80, updateAllDataCallback);
HardwareService hardwareService(handleSingleClick, handleDoubleClick, handleLongPress);
TimeService timeService;
CalendarService calendarService;
WeatherService weatherService;
AirQualityService airQualityService;
DaylightService daylightService;
MoonService moonService;
PopulationService populationService;
CryptoService cryptoService;
CurrencyService currencyService;
StockService stockService;
PcMonitorService pcMonitorService;
BambuService bambuService;
DataSyncService dataSyncService;
NightModeService nightModeService;

unsigned long lastScreenSwitch = 0;

// Brightness schedule: a manual slider change overrides the active slot until the next one starts.
int activeBrightnessSlot = -1;
bool brightnessOverridden = false;

// Core Application Logic

void handleScreenNavigation(bool goToPrevious) {
  int activeAction = TimeService::getActiveNightAction(appState.config);
  bool wasScreenOff = nightModeService.wasScreenOff(activeAction);

  if (wasScreenOff) {
    Serial.println("🌙 Night Mode: Waking display temporarily on Primary Screen.");
    displayService.jumpToFirstEnabledScreen(appState);
    displayService.setContrast(true);
  } else {
    if (nightModeService.isLatched()) {
      displayService.setContrast(activeAction != 0);
    }
    if (goToPrevious) {
      Serial.println("👆👆 Double Click: Switching to Previous Screen");
      displayService.switchToPreviousScreen(appState);
    } else {
      Serial.println("👆 Single Click: Switching to Next Screen");
      displayService.switchToNextScreen(appState);
    }
  }

  lastScreenSwitch = millis();
  nightModeService.recordInteraction();
}

void handleSingleClick() {
  handleScreenNavigation(false);
}

void handleDoubleClick() {
  handleScreenNavigation(true);
}

void handleLongPress() {
  appState.config.screen_auto_cycle = !appState.config.screen_auto_cycle;

  if (appState.config.screen_auto_cycle) {
    Serial.println("🔄 Auto Cycle: ENABLED");
    displayService.drawInfoScreen(icon_unlock, "Auto Cycle On");
    displayService.display.display();
  } else {
    Serial.println("🔒 Auto Cycle: DISABLED (Screen Locked)");
    displayService.drawInfoScreen(icon_lock, "Auto Cycle Off");
    displayService.display.display();
  }
  
  configManager.saveConfig(appState.config);

  delay(1000);

  lastScreenSwitch = millis();
  nightModeService.recordInteraction();
}

// Brightness in effect now: the scheduled slot unless the user overrode it.
int effectiveBrightness() {
  int slot = TimeService::getActiveBrightnessSlot(appState.config);
  if (slot != activeBrightnessSlot) {
    activeBrightnessSlot = slot;
    brightnessOverridden = false;
    if (slot >= 0) {
      Serial.printf("🔆 Brightness schedule: %s -> %d%%\n", appState.config.bright_times[slot].c_str(), appState.config.bright_levels[slot]);
    }
  }
  if (slot < 0 || brightnessOverridden) return appState.config.brightness;
  return appState.config.bright_levels[slot];
}

// Live brightness change from the Web Panel or PC App.
// Applied immediately; written to flash only when the slider is released.
void applyBrightness(int percent, bool persist) {
  appState.config.brightness = constrain(percent, 1, 100);
  brightnessOverridden = (activeBrightnessSlot >= 0);
  displayService.setBrightness(appState.config.brightness);
  if (!nightModeService.isLatched()) {
    displayService.setContrast(false);
  }
  if (persist) {
    configManager.saveConfig(appState.config);
  }
}

// Full Data Sync
void updateAllData() {
  nightModeService.reset();
  brightnessOverridden = false;  // A saved schedule takes effect right away
  dataSyncService.runFullSync(appState);
  displayService.jumpToFirstEnabledScreen(appState);
  lastScreenSwitch = millis();
}

// Global function wrapper for the class method
void updateAllDataCallback() {
  updateAllData();
}

void setup() {
  Serial.setRxBufferSize(1024);
  Serial.begin(115200);
  delay(100);

  configManager.loadConfig(appState.config);
  hardwareService.begin(appState.config);

  displayService.begin(appState.config.sda_pin, appState.config.scl_pin);
  displayService.setBrightness(appState.config.brightness);
  displayService.setContrast(false);
  displayService.setInverted(appState.config.invert_display);
  pcMonitorService.setBrightnessCallback(applyBrightness);
  delay(3000);

  displayService.showOLEDStatus({"\n", "\n", "Starting...", "\n", "\n", "Config Loaded!"}, true);
  bambuService.begin(&appState.config, &appState.bambu);

  WiFiManager wm;
  wm.setConnectTimeout(15);
  wm.setConnectRetries(3);

  wm.setAPCallback([](WiFiManager* m) {
    displayService.showOLEDStatus({"\n", "WiFi not connected", "\n", "Connect to WiFi:", AP_SSID, "\n", "Password:", AP_PASS}, true);
  });

  displayService.showOLEDStatus({"\n", "\n", "Connecting...", "\n", "\n", "Searching WiFi..."}, true);

  if (wm.autoConnect(AP_SSID, AP_PASS)) {
    String ipAddress = WiFi.localIP().toString();
    String mac = WiFi.macAddress();
    mac.replace(":", "");
    String uniqueName = "tinytosh-" + mac.substring(8);
    uniqueName.toLowerCase();

    Serial.println("WiFi Connected!"); 
    Serial.print("IP Address: "); 
    Serial.println(ipAddress);

    appState.config.device_id = uniqueName;
    appState.config.ip_address = ipAddress;
    displayService.showOLEDStatus({
        "Connected to WiFi!", 
        "", 
        "IP: " + ipAddress, 
        "Name: " + uniqueName, 
        "", 
        "Loading..."
    }, true);

    delay(3000); 

    // 4. Initial Data Fetch (Synchronous)
    updateAllData();

  } else {
    Serial.println("Failed to connect and timed out. Staying in AP Mode.");
    displayService.showOLEDStatus({"\n", "Connect Failed!", "\n", "Use Web Panel to set WiFi."}, true);
  }

  // 5. Initialize Web Server
  webServerService.setAppState(&appState);
  webServerService.setBrightnessCallback(applyBrightness);
  webServerService.begin();
}

void loop() {
  webServerService.handleClient();
  bambuService.loop();
  hardwareService.tick();

  if (pcMonitorService.handleSerial(appState)) {
    Serial.println("Config updated via USB! Saving and applying...");
    configManager.saveConfig(appState.config);
    updateAllData();
  }

  // Claude Code: bring its screen forward the moment Claude is blocked on the user.
  // This overrides Night Mode: the alert wakes the display at normal brightness.
  static bool claudeWasWaiting = false;
  static bool claudeAlertDuringNight = false;
  bool claudeWaiting = appState.config.claude_alert
                       && ClaudeService::needsUser(appState.claude)
                       && displayService.isScreenEnabled(appState, SCREEN_CLAUDE);
  if (claudeWaiting && !claudeWasWaiting) {
    displayService.jumpToScreen(appState, SCREEN_CLAUDE);
    lastScreenSwitch = millis();
  }
  if (claudeWaiting && nightModeService.isLatched()) {
    claudeAlertDuringNight = true;
  }
  if (!claudeWaiting && claudeWasWaiting && claudeAlertDuringNight) {
    // Answered at night: go back to the primary screen instead of staying on Claude all night.
    claudeAlertDuringNight = false;
    if (nightModeService.isLatched()) displayService.jumpToFirstEnabledScreen(appState);
  }
  claudeWasWaiting = claudeWaiting;

  // 1. Night Latch Logic
  int activeAction = TimeService::getActiveNightAction(appState.config);
  bool justExitedNightMode = nightModeService.update(activeAction, displayService.isOnFirstEnabledScreen(appState));

  if (justExitedNightMode) {
    // If we are exiting mode 2 or 3 (display was off), reset to the main screen
    if (appState.config.night_action >= 2) {
      displayService.jumpToFirstEnabledScreen(appState);
    }
    lastScreenSwitch = millis();
  }

  // 2. Scheduled Data Refresh (Non-Blocking via FreeRTOS Task)
  dataSyncService.maybeStartBackgroundSync(appState, nightModeService.isLatched());

  // 3. Auto Screen Switching Logic
  bool holdOnClaude = claudeWaiting && displayService.getCurrentScreen() == SCREEN_CLAUDE;
  if (appState.config.screen_auto_cycle && !nightModeService.isLatched() && !holdOnClaude) {
    unsigned long intervalMs = appState.config.screen_interval_sec * 1000;

    if (millis() - lastScreenSwitch >= intervalMs) {
      displayService.switchToNextScreen(appState);
      lastScreenSwitch = millis();
    }
  }

  // 4. Screen Redraw & Visual Action Logic
  static bool screenClearedForNight = false;

  bool isTemporarilyAwake = nightModeService.isTemporarilyAwake(activeAction);
  bool shouldDrawScreen = !nightModeService.isScreenOffAction(activeAction) || isTemporarilyAwake || claudeWaiting;

  if (!shouldDrawScreen) {
    if (!screenClearedForNight) {
      displayService.setInverted(false);  // An inverted blank screen would light every pixel
      displayService.display.clearDisplay();
      displayService.display.display();
      screenClearedForNight = true;
      Serial.println("💤 Night Mode: Display turned OFF to save power. Waiting for interaction or morning.");
    }
  } else {
    if (screenClearedForNight) {
      screenClearedForNight = false;
      Serial.println("💡 Night Mode: Display turned back ON.");
    }

    // Night Mode redraws only every 10-60 s; a pending alert keeps the normal 1 s pace.
    const unsigned long ALERT_REFRESH_MS = 1000;
    unsigned long refreshInterval = claudeWaiting ? ALERT_REFRESH_MS : nightModeService.getRefreshIntervalMs(activeAction);

    if (nightModeService.isRedrawDue(refreshInterval)) {
      displayService.setBrightness(effectiveBrightness());
      displayService.setInverted(appState.config.invert_display);
      if (claudeWaiting) {
        displayService.setContrast(false);  // Normal brightness, even at night
      } else if (nightModeService.isLatched()) {
        if (activeAction == 1 || isTemporarilyAwake) {
          displayService.setContrast(true);
        } else if (activeAction == 0) {
          displayService.setContrast(false);
        }
      } else {
        displayService.setContrast(false);
      }

      displayService.drawCurrentScreen(appState);
      displayService.display.display();
      nightModeService.markRedrawn();
    }
  }
}