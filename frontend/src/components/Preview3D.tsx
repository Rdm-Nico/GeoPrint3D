import { Suspense, useRef } from 'react';
import { Canvas, useFrame } from '@react-three/fiber';
import { OrbitControls, PerspectiveCamera, Grid } from '@react-three/drei';
import type { Mesh } from 'three';

interface Preview3DProps {
  stlUrl?: string;
  stats?: {
    vertices: number;
    triangles: number;
    min_elevation: number;
    max_elevation: number;
  };
}

function TerrainPlaceholder() {
  const meshRef = useRef<Mesh>(null);

  useFrame((state) => {
    if (meshRef.current) {
      meshRef.current.rotation.z = Math.sin(state.clock.elapsedTime * 0.5) * 0.1;
    }
  });

  return (
    <mesh ref={meshRef} rotation={[-Math.PI / 2, 0, 0]}>
      <planeGeometry args={[5, 5, 20, 20]} />
      <meshStandardMaterial
        color="#4ade80"
        wireframe={false}
        roughness={0.8}
        metalness={0.2}
      />
    </mesh>
  );
}

function LoadingSpinner() {
  return (
    <div className="flex items-center justify-center h-full">
      <div className="animate-spin rounded-full h-12 w-12 border-t-2 border-b-2 border-blue-500"></div>
    </div>
  );
}

export default function Preview3D({ stlUrl, stats }: Preview3DProps) {
  return (
    <div className="w-full h-full bg-gradient-to-br from-gray-900 to-gray-800 rounded-lg overflow-hidden">
      <Canvas>
        <PerspectiveCamera makeDefault position={[8, 8, 8]} fov={50} />
        <OrbitControls
          enablePan={true}
          enableZoom={true}
          enableRotate={true}
          minDistance={3}
          maxDistance={20}
        />

        {/* Lighting */}
        <ambientLight intensity={0.4} />
        <directionalLight position={[10, 10, 5]} intensity={1} castShadow />
        <directionalLight position={[-10, -10, -5]} intensity={0.3} />
        <hemisphereLight intensity={0.3} />

        <Suspense fallback={<LoadingSpinner />}>
          {stlUrl ? (
            // TODO: Load actual STL file when backend provides it
            // <STLModel url={stlUrl} />
            <TerrainPlaceholder />
          ) : (
            <TerrainPlaceholder />
          )}
        </Suspense>

        {/* Ground grid */}
        <Grid
          args={[10, 10]}
          cellSize={0.5}
          cellThickness={0.5}
          cellColor="#6b7280"
          sectionSize={2}
          sectionThickness={1}
          sectionColor="#374151"
          fadeDistance={25}
          fadeStrength={1}
          followCamera={false}
          position={[0, -0.01, 0]}
        />
      </Canvas>

      {/* Stats overlay */}
      {stats && (
        <div className="absolute bottom-4 left-4 bg-black bg-opacity-70 text-white p-3 rounded text-xs space-y-1">
          <div>Vertices: {stats.vertices.toLocaleString()}</div>
          <div>Triangles: {stats.triangles.toLocaleString()}</div>
          <div>Elevation: {stats.min_elevation.toFixed(1)}m - {stats.max_elevation.toFixed(1)}m</div>
        </div>
      )}

      {/* Controls info */}
      <div className="absolute top-4 right-4 bg-black bg-opacity-70 text-white p-3 rounded text-xs space-y-1">
        <div>🖱️ Left click + drag: Rotate</div>
        <div>🖱️ Right click + drag: Pan</div>
        <div>🖱️ Scroll: Zoom</div>
      </div>
    </div>
  );
}
