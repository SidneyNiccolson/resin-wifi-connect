#!/usr/bin/env bash

# Enable i2c - needed for sensors
modprobe i2c-dev
modprobe w1-gpio && modprobe w1-therm

export DBUS_SYSTEM_BUS_ADDRESS=unix:path=/host/run/dbus/system_bus_socket

# Choose a condition for running WiFi Connect according to your use case:

# 1. Is there a default gateway?
# ip route | grep default

# 2. Is there Internet connectivity?
# nmcli -t g | grep full

# 3. Is there Internet connectivity via a google ping?
# wget --spider http://google.com 2>&1

# 4. Is there an active WiFi connection?
iwgetid -r

if [ $? -eq 0 ]; then
    printf 'Skipping WiFi Connect\n'
    # printf 'Starting pigpiod\n'
    # sudo pigpiod
    # printf 'Starting core.py\n'
    # cd astroplant-kit/astroplant_kit && python3 core.py
else
    printf 'Starting WiFi Connect\n'
    ./wifi-connect
fi

# Start your application here.

