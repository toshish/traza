import React from 'react';
import { createRoot } from 'react-dom/client';
import './styles.css';
import { App } from './App.jsx';
import { consumeFragmentToken } from './lib/handoff.js';

// Before the first render, so the hash router never sees the token and the
// first request already carries it.
consumeFragmentToken();

createRoot(document.getElementById('root')).render(<App />);
