---
name: weather
description: "Get current weather conditions and forecasts for any location."
keywords: [weather, forecast, temperature, rain, wind]
---

# Weather Skill

Use the web_fetch tool to get weather information from wttr.in.

## When to Use

- User asks about weather, temperature, humidity, or wind
- User wants to know if it will rain
- User asks about weather forecast for the coming days

## How to Use

Fetch weather data:
- Current: `https://wttr.in/{location}?format=3`
- Detailed: `https://wttr.in/{location}?format=%l:+%c+%t+%h+%w`
- JSON: `https://wttr.in/{location}?format=j1`
