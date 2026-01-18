import { useState } from 'react';
import MapSelector from './components/MapSelector';
import Preview3D from './components/Preview3D';
import { api } from './services/api';
import type { BoundingBox, GenerateResponse } from './types';

export default function App() {
  const [selectedBounds, setSelectedBounds] = useState<BoundingBox | null>(null);
  const [generating, setGenerating] = useState(false);
  const [result, setResult] = useState<GenerateResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

  // Advanced settings
  const [resolution, setResolution] = useState(100);
  const [verticalScale, setVerticalScale] = useState(1.5);
  const [baseHeight, setBaseHeight] = useState(2.0);
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

        {/* Preview & Controls panel */}
        <div className="w-full md:w-1/2 h-[50vh] md:h-full flex flex-col bg-gray-50">
          {/* Preview */}
          <div className="flex-1 p-4">
            <Preview3D
              stlUrl={result?.preview_url}
              stats={result?.stats}
            />
          </div>

          {/* Control panel */}
          <div className="p-4 bg-white border-t space-y-4">
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
                    max="200"
                    step="10"
                    value={resolution}
                    onChange={(e) => setResolution(Number(e.target.value))}
                    className="w-full"
                  />
                </div>

                <div>
                  <label className="block text-xs font-medium text-gray-700 mb-1">
                    Vertical Scale: {verticalScale}x
                  </label>
                  <input
                    type="range"
                    min="0.5"
                    max="3"
                    step="0.1"
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

            {/* Status message */}
            {selectedBounds && (
              <div className="text-xs text-green-600 bg-green-50 p-2 rounded">
                ✓ Area selected ({(result?.stats.area_km2 || 0).toFixed(4)} km²)
              </div>
            )}

            {error && (
              <div className="text-xs text-red-600 bg-red-50 p-2 rounded">
                ✗ {error}
              </div>
            )}

            {/* Generate button */}
            <button
              onClick={handleGenerate}
              disabled={!selectedBounds || generating}
              className={`w-full py-3 px-6 rounded-lg font-semibold text-white transition-colors ${
                !selectedBounds || generating
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

            {/* Download button */}
            {result && (
              <button
                onClick={() => api.downloadSTL(result.stl_url)}
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
