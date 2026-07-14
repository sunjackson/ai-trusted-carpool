# UI Design Source

`ui-design-board-v4.png` is the current ImageGen-generated product design board for the six core flows: welcome, one-click host setup, live host statistics, invite join, tool choice, and active ride.

Implementation rules:
- one primary action per onboarding page;
- no P2P, WebRTC, or TURN terminology in normal views;
- warm gold primary actions, emerald success, charcoal desktop surfaces;
- shared responsive layout for macOS, Windows, and Linux;
- host schedule always uses an explicit start/end range;
- live usage groups person → tool → model, with requests, input, output, cache read/write, Claude cache TTL writes, and official USD price estimates;
- credentials remain on the host device, and invite codes expire with the car schedule.
