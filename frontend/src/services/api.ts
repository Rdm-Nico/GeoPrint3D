import axios from 'axios';
import type { GenerateRequest, GenerateResponse } from '../types';

const API_BASE_URL = '/api';

export const api = {
  /**
   * Check backend health
   */
  healthCheck: async () => {
    const response = await axios.get(`${API_BASE_URL}/health`);
    return response.data;
  },

  /**
   * Generate 3D terrain model from bounding box
   */
  generateTerrain: async (request: GenerateRequest): Promise<GenerateResponse> => {
    const response = await axios.post<GenerateResponse>(
      `${API_BASE_URL}/generate`,
      request,
      {
        timeout: 120000, // 2 minutes timeout for heavy processing
      }
    );
    return response.data;
  },

  /**
   * Download STL file
   */
  downloadSTL: (url: string) => {
    window.open(url, '_blank');
  },
};
