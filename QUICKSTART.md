# GeoPrint3D - Quick Start Guide

Get up and running in 5 minutes!

## Prerequisites

Install these tools:

### 1. Rust
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### 2. Node.js (20+)
```bash
# Visit https://nodejs.org/ or use nvm
curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.39.0/install.sh | bash
nvm install 20
```

### 3. PROJ Library

**macOS:**
```bash
brew install proj
```

**Ubuntu/Debian:**
```bash
sudo apt-get install libproj-dev
```

**Windows:**
Download from https://proj.org/install.html

## Installation

### 1. Clone or Navigate to Project
```bash
cd 3D-World-Printer
```

### 2. Install Backend Dependencies
```bash
cd backend
cargo build
```

This will take a few minutes the first time as it downloads and compiles all Rust crates.

### 3. Install Frontend Dependencies
```bash
cd ../frontend
npm install
```

## Running the Application

### Terminal 1: Start Backend
```bash
cd backend
cargo run
```

You should see:
```
🚀 GeoPrint3D Backend starting on 0.0.0.0:8080
```

### Terminal 2: Start Frontend
```bash
cd frontend
npm run dev
```

You should see:
```
VITE v5.x.x  ready in xxx ms

➜  Local:   http://localhost:3000/
```

## Using the Application

1. **Open your browser** to http://localhost:3000

2. **Select an area on the map:**
   - Click and hold on the map
   - Drag to create a bounding box
   - Release to confirm (must be ≤ 1 km²)

3. **(Optional) Adjust settings:**
   - Click "Show Advanced Settings"
   - Adjust resolution, vertical scale, base height
   - Toggle buildings on/off

4. **Click "Generate 3D Model"**
   - Wait for processing (10-60 seconds typically)
   - Preview appears in 3D viewer

5. **Download the STL file**
   - Click "Download STL File"
   - Import into your 3D printer slicer

## Troubleshooting

### Backend won't start
```
error: failed to run custom build command for `proj-sys`
```
→ Install PROJ library (see Prerequisites)

### Frontend can't connect to backend
```
Network Error
```
→ Make sure backend is running on port 8080

### Port already in use
```
Error: Address already in use
```
→ Kill the process:
```bash
lsof -ti:8080  # Find process ID
kill -9 <PID>  # Kill it
```

## Next Steps

- **Read the full README.md** for detailed documentation
- **Check ARCHITECTURE.md** to understand the system design
- **See DEVELOPMENT.md** for development workflows
- **Review ASYNC_COMMUNICATION.md** for implementation details

## Example Locations to Try

- **New York City**: 40.7580° N, 73.9855° W
- **San Francisco**: 37.7749° N, 122.4194° W
- **Tokyo**: 35.6762° N, 139.6503° E
- **Paris**: 48.8566° N, 2.3522° E
- **Sydney**: -33.8688° S, 151.2093° E

## Tips

1. **Start small**: Try a 0.25 km² area first to test
2. **Urban areas**: Enable buildings for interesting models
3. **Mountains**: Increase vertical scale (2-3x) for dramatic effect
4. **3D Printing**:
   - Scale appropriately in your slicer
   - Default base height (2mm) works well for most printers
   - Print with supports if needed

## Getting Help

- Check the documentation files
- Look at the code comments
- Review the example implementations

Happy printing! 🖨️🌍
