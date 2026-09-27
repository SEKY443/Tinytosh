#ifndef DISPLAY_SERVICE_H
#define DISPLAY_SERVICE_H

#include <Adafruit_SSD1306.h>
#include <Wire.h>

#include "structs.h"

class DisplayService {
public:
    Adafruit_SSD1306 display;

    DisplayService(int width, int height, int reset_pin);
    void begin(int sda, int scl);
    
    void showOLEDStatus(std::initializer_list<String> lines, bool clear = true);
    void drawTimeScreen(const Config& config, String timeStr, String dateStr);
    void drawCalendarScreen(const Config& config, const CalendarData& calendar);
    void drawWeatherScreen(const Config& config, const WeatherData& data, const String& currentTime);
    void drawAQIScreen(const Config& config, const AirQualityData& data, const String& currentTime);
    void drawDaylightScreen(const Config& config, const DaylightData& data);
    void drawMoonScreen(const Config& config, const MoonData& data);
    void drawPopulationScreen(const Config& config, const PopulationData& data);
    void drawCryptoScreen(const Config& config, const CryptoData& data);
    void drawCurrencyScreen(const Config& config, const CurrencyData& data, int multiplier);
    void drawStockScreen(const Config& config, const StockData& data);
    void drawPcScreen(const PcStats& pcStats);
    void drawMediaScreen(const PcMedia& media);
    void drawBambuScreen(const BambuData& bambu);
    void drawClaudeScreen(const ClaudeData& claude);
    void drawInfoScreen(const unsigned char* image = nullptr, String text = "No Data");

    void drawScreen(int screenIndex, const AppState& state, int subIndex = 0);
    void animateTransition(int prevScreen, int prevSub, int nextScreen, int nextSub, const AppState& state);

    bool isScreenEnabled(const AppState& state, int screenIndex);
    void drawCurrentScreen(const AppState& state);
    void switchToNextScreen(const AppState& state);
    void switchToPreviousScreen(const AppState& state);
    void jumpToFirstEnabledScreen(const AppState& state);
    void jumpToScreen(const AppState& state, int screenIndex);
    int getCurrentScreen() const;
    bool isOnFirstEnabledScreen(const AppState& state);

    void setContrast(bool dim);
    void setBrightness(int percent);

private:
    uint8_t screenBufferOld[1024];
    uint8_t screenBufferNew[1024];

    int currentScreen = 0;
    int currentSubScreen = 0;

    static const int CONTRAST_MIN = 1;
    static const int CONTRAST_MAX = 255;

    // Contrast alone bottoms out quite bright on SSD1306. The low band also
    // shortens the pixel pre-charge and lowers VCOMH for a much darker floor.
    static const int LOW_DRIVE_MAX_PERCENT = 30;   // Web Panel and PC App show "CRT" up to this value
    static const uint8_t PRECHARGE_NORMAL = 0xF1;  // Adafruit default for SWITCHCAPVCC
    static const uint8_t PRECHARGE_LOW = 0x11;
    static const uint8_t VCOMH_NORMAL = 0x40;      // Adafruit default
    static const uint8_t VCOMH_LOW = 0x00;

    int brightnessPercent = 100;
    int appliedLevel = -1;

    void applyDriveLevel(int percent);

    int getFirstEnabledScreen(const AppState& state);

    int getNextAnimationEffect(uint16_t mask);
    void animateHorizontal(int prev, int pSub, int next, int nSub, const AppState& state);
    void animateVertical(int prev, int pSub, int next, int nSub, const AppState& state);
    void animateDissolve(int prev, int pSub, int next, int nSub, const AppState& state);
    void animateCurtain(int prev, int pSub, int next, int nSub, const AppState& state);
    void animateBlinds(int prev, int pSub, int next, int nSub, const AppState& state);

    const unsigned char* getWeatherBitmap(int wmo_code, bool is_day);
    const unsigned char* getAQIBitmap(int val, bool is_eu);

    void drawClaudeAlert(const ClaudeData& claude);
    void drawUsageRow(int y, const char* label, int percent, int resetMinutes);
    static String fitText(String text, int maxChars);
};

#endif