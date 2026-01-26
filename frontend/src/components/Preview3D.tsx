import { Suspense, useRef, useMemo } from 'react';
import { Canvas, useFrame } from '@react-three/fiber';
import { OrbitControls, PerspectiveCamera, Grid, Center } from '@react-three/drei';
import * as THREE from 'three';
import type { Mesh } from 'three';
import type { MeshData, MeshStats } from '../types';

interface Preview3DProps {
  meshData?: MeshData;
  stats?: MeshStats;
}

// Component that renders the actual terrain mesh from vertices and triangles
function TerrainMesh({ meshData }: { meshData: MeshData }) {
  const meshRef = useRef<Mesh>(null);

  // Create the geometry from vertices and triangles
  const geometry = useMemo(() => {
    const geo = new THREE.BufferGeometry();

    // Flatten vertices array: [[x,y,z], [x,y,z], ...] -> [x,y,z,x,y,z,...]
    const positions = new Float32Array(meshData.vertices.flat());

    // Flatten triangles array: [[a,b,c], [a,b,c], ...] -> [a,b,c,a,b,c,...]
    const indices = new Uint32Array(meshData.triangles.flat());

    geo.setAttribute('position', new THREE.BufferAttribute(positions, 3));
    geo.setIndex(new THREE.BufferAttribute(indices, 1));

    // Compute normals for proper lighting
    geo.computeVertexNormals();

    // Center the geometry
    geo.computeBoundingBox();
    if (geo.boundingBox) {
      const center = new THREE.Vector3();
      geo.boundingBox.getCenter(center);
      geo.translate(-center.x, -center.y, -center.z);
    }

    // Scale to fit nicely in the view (normalize to ~5 units)
    geo.computeBoundingBox();
    if (geo.boundingBox) {
      const size = new THREE.Vector3();
      geo.boundingBox.getSize(size);
      const maxDim = Math.max(size.x, size.y, size.z);
      if (maxDim > 0) {
        const scale = 5 / maxDim;
        geo.scale(scale, scale, scale);
      }
    }

    return geo;
  }, [meshData]);

  return (
    <mesh ref={meshRef} geometry={geometry} rotation={[-Math.PI / 2, 0, 0]}>
      <meshStandardMaterial
        color="#4ade80"
        side={THREE.DoubleSide}
        roughness={0.7}
        metalness={0.1}
        flatShading={true}
      />
    </mesh>
  );
}

// Placeholder shown when no mesh data is available
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

export default function Preview3D({ meshData, stats }: Preview3DProps) {
  return (
    <div className="w-full h-full bg-gradient-to-br from-gray-900 to-gray-800 rounded-lg overflow-hidden relative">
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

        <Suspense fallback={null}>
          <Center>
            {meshData && meshData.vertices.length > 0 ? (
              <TerrainMesh meshData={meshData} />
            ) : (
              <TerrainPlaceholder />
            )}
          </Center>
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

      {/* Status indicator */}
      <div className="absolute top-4 left-4 bg-black bg-opacity-70 text-white px-3 py-1 rounded text-sm">
        {meshData && meshData.vertices.length > 0 ? (
          <span className="text-green-400">3D Model Loaded</span>
        ) : (
          <span className="text-yellow-400">Select an area to generate</span>
        )}
      </div>

      {/* Stats overlay */}
      {stats && (
        <div className="absolute bottom-4 left-4 bg-black bg-opacity-70 text-white p-3 rounded text-xs space-y-1">
          <div>Vertices: {stats.vertices.toLocaleString()}</div>
          <div>Triangles: {stats.triangles.toLocaleString()}</div>
          <div>Buildings: {stats.buildings_count.toLocaleString()}</div>
          <div>Elevation: {stats.min_elevation.toFixed(1)}m - {stats.max_elevation.toFixed(1)}m</div>
          <div>Area: {stats.area_km2.toFixed(4)} km²</div>
        </div>
      )}

      {/* Controls info */}
      <div className="absolute top-4 right-4 bg-black bg-opacity-70 text-white p-3 rounded text-xs space-y-1">
        <div>Left click + drag: Rotate</div>
        <div>Right click + drag: Pan</div>
        <div>Scroll: Zoom</div>
      </div>
    </div>
  );
}
