import { useState } from 'react';
import MapSelector from './components/MapSelector';
import Preview3D from './components/Preview3D';
import { api } from './services/api';
import type { BoundingBox, GenerateResponse } from './types';

function computeArea(bbox: BoundingBox): number {
  const latKm = (bbox.max_lat - bbox.min_lat) * 111.0;
  const lonKm =
    (bbox.max_lon - bbox.min_lon) * 111.0 * Math.cos((bbox.min_lat * Math.PI) / 180);
  return Math.abs(latKm * lonKm);
}

type AreaStatus = 'green' | 'yellow' | 'red';

function areaStatus(km2: number): AreaStatus {
  if (km2 > 20) return 'red';
  if (km2 > 10) return 'yellow';
  return 'green';
}

export default function App() {
  const [selectedBounds, setSelectedBounds] = useState<BoundingBox | null>(null);
  const [generating, setGenerating] = useState(false);
  const [result, setResult] = useState<GenerateResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

  // Advanced settings
  const [resolution, setResolution] = useState(150);
  const [verticalScale, setVerticalScale] = useState(2.0);
  const [baseHeight, setBaseHeight] = useState(2.0);
  const [printSize, setPrintSize] = useState(180);
  const [includeBuildings, setIncludeBuildings] = useState(true);
  const [showSettings, setShowSettings] = useState(false);

  const handleGenerate = async () => {
    if (!selectedBounds) {
      setError('Please select an area on the map first');
      return;
    }

    setGenerating(true);
    setError(null);
    setResult(null);

    try {
      const response = await api.generateTerrain({
        bbox: selectedBounds,
        resolution,
        vertical_scale: verticalScale,
        base_height: baseHeight,
        include_buildings: includeBuildings,
        print_size: printSize,
      });

      setResult(response);
    } catch (err: any) {
      setError(err.response?.data?.error || err.message || 'Failed to generate terrain');
    } finally {
      setGenerating(false);
    }
  };

  return (
    <div className="h-screen w-screen flex flex-col bg-gray-100">
      {/* Header */}
      <header className="bg-gradient-to-r from-blue-600 to-blue-800 text-white p-4 shadow-lg">
        <div className="container mx-auto">
          <h1 className="text-2xl md:text-3xl font-bold">GeoPrint3D</h1>
          <p className="text-sm md:text-base text-blue-100">
            Transform real-world terrain into 3D-printable models
          </p>
        </div>
      </header>

      {/* Main content */}
      <main className="flex-1 flex flex-col md:flex-row overflow-hidden">
        {/* Map panel */}
        <div className="w-full md:w-1/2 h-[50vh] md:h-full relative">
          <MapSelector onBoundsChange={setSelectedBounds} />
        </div>

        {/* Preview & Controls panel — scrollable so controls are always reachable */}
        <div className="w-full md:w-1/2 h-[50vh] md:h-full flex flex-col bg-gray-50 overflow-y-auto">
          {/* Preview — fixed height so it doesn't collapse when controls expand */}
          <div className="flex-shrink-0 h-[25vh] md:h-[42vh] p-4">
            <Preview3D
              meshData={result?.mesh_data}
              stats={result?.stats}
            />
          </div>

          {/* Control panel */}
          <div className="flex-shrink-0 p-4 bg-white border-t space-y-4">
            {/* Settings toggle */}
            <button
              onClick={() => setShowSettings(!showSettings)}
              className="text-sm text-blue-600 hover:text-blue-800 font-medium"
            >
              {showSettings ? '▼ Hide' : '▶ Show'} Advanced Settings
            </button>

            {/* Advanced settings */}
            {showSettings && (
              <div className="grid grid-cols-2 gap-4 p-4 bg-gray-50 rounded">
                <div>
                  <label className="block text-xs font-medium text-gray-700 mb-1">
                    Resolution: {resolution}x{resolution}
                  </label>
                  <input
                    type="range"
                    min="50"
                    max="250"
                    step="10"
                    value={resolution}
                    onChange={(e) => setResolution(Number(e.target.value))}
                    className="w-full"
                  />
                </div>

                <div>
                  <label className="block text-xs font-medium text-gray-700 mb-1">
                    Vertical Exaggeration: {verticalScale}x (1x = ~15% height)
                  </label>
                  <input
                    type="range"
                    min="0.5"
                    max="10"
                    step="0.5"
                    value={verticalScale}
                    onChange={(e) => setVerticalScale(Number(e.target.value))}
                    className="w-full"
                  />
                </div>

                <div>
                  <label className="block text-xs font-medium text-gray-700 mb-1">
                    Base Height: {baseHeight}mm
                  </label>
                  <input
                    type="range"
                    min="1"
                    max="5"
                    step="0.5"
                    value={baseHeight}
                    onChange={(e) => setBaseHeight(Number(e.target.value))}
                    className="w-full"
                  />
                </div>

                <div>
                  <label className="block text-xs font-medium text-gray-700 mb-1">
                    Print Size: {printSize}mm
                  </label>
                  <input
                    type="range"
                    min="80"
                    max="250"
                    step="10"
                    value={printSize}
                    onChange={(e) => setPrintSize(Number(e.target.value))}
                    className="w-full"
                  />
                </div>

                <div className="flex items-center">
                  <label className="flex items-center text-xs font-medium text-gray-700">
                    <input
                      type="checkbox"
                      checked={includeBuildings}
                      onChange={(e) => setIncludeBuildings(e.target.checked)}
                      className="mr-2"
                    />
                    Include Buildings
                  </label>
                </div>
              </div>
            )}

            {/* Area semaphore */}
            {selectedBounds && (() => {
              const km2 = computeArea(selectedBounds);
              const status = areaStatus(km2);
              const styles = {
                green:  { wrap: 'text-green-700 bg-green-50 border border-green-200',  icon: '🟢' },
                yellow: { wrap: 'text-yellow-700 bg-yellow-50 border border-yellow-200', icon: '🟡' },
                red:    { wrap: 'text-red-700 bg-red-50 border border-red-200',         icon: '🔴' },
              }[status];
              return (
                <div className={`text-xs p-2 rounded ${styles.wrap}`}>
                  <div className="flex items-center gap-1 font-medium">
                    {styles.icon} Area selected: {km2.toFixed(4)} km²
                  </div>
                  {status === 'yellow' && (
                    <div className="mt-0.5 opacity-80">
                      Near the 20 km² limit — generation may be slow.
                    </div>
                  )}
                  {status === 'red' && (
                    <div className="mt-0.5 font-semibold">
                      Exceeds the 20 km² limit. Please select a smaller area before generating.
                    </div>
                  )}
                </div>
              );
            })()}

            {error && (
              <div className="text-xs text-red-600 bg-red-50 p-2 rounded">
                ✗ {error}
              </div>
            )}

            {/* Generate button */}
            {(() => {
              const overLimit = selectedBounds ? areaStatus(computeArea(selectedBounds)) === 'red' : false;
              const disabled = !selectedBounds || generating || overLimit;
              return (
              <button
                onClick={handleGenerate}
                disabled={disabled}
                title={overLimit ? 'Area exceeds 20 km² — reduce selection to generate' : undefined}
                className={`w-full py-3 px-6 rounded-lg font-semibold text-white transition-colors ${
                  disabled
                    ? 'bg-gray-400 cursor-not-allowed'
                    : 'bg-blue-600 hover:bg-blue-700 active:bg-blue-800'
                }`}
              >
              {generating ? (
                <span className="flex items-center justify-center">
                  <svg className="animate-spin -ml-1 mr-3 h-5 w-5 text-white" xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24">
                    <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4"></circle>
                    <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"></path>
                  </svg>
                  Generating 3D Model...
                </span>
              ) : (
                'Generate 3D Model'
              )}
            </button>
              );
            })()}

            {/* Download button */}
            {result && result.mesh_data && (
              <button
                onClick={() => api.downloadSTL(result.mesh_data, `terrain_${Date.now()}.stl`)}
                className="w-full py-3 px-6 rounded-lg font-semibold text-white bg-green-600 hover:bg-green-700 transition-colors"
              >
                Download STL File
              </button>
            )}

            {/* Stats */}
            {result && (
              <div className="bg-blue-50 p-3 rounded text-xs space-y-1">
                <div className="font-semibold text-blue-900">Model Statistics:</div>
                <div className="grid grid-cols-2 gap-2 text-gray-700">
                  <div>Vertices: {result.stats.vertices.toLocaleString()}</div>
                  <div>Triangles: {result.stats.triangles.toLocaleString()}</div>
                  <div>Buildings: {result.stats.buildings_count}</div>
                  <div>Area: {result.stats.area_km2.toFixed(4)} km²</div>
                </div>
              </div>
            )}
          </div>
        </div>
      </main>
    </div>
  );
}
