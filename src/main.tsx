import React from 'react';
import ReactDOM from 'react-dom/client';
import App from './App';
import { installDebugCapture } from './debugLog';
import './styles.css';

installDebugCapture();

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>
);
