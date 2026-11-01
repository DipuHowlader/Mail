import React from 'react';

export default function AppLogo({ className = '' }) {
  return (
    <span className={`flex shrink-0 items-center justify-center overflow-hidden ${className}`}>
      <img
        src="/novamail.png"
        alt=""
        aria-hidden="true"
        draggable="false"
        className="h-full w-full scale-150 object-contain"
      />
    </span>
  );
}
