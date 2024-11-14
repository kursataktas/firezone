//
//  main.swift
//  FirezoneNetworkExtension
//
//  Created by Jamil Bou Kheir on 11/14/24.
//
//  Used only for the Standalone macOS app

import Foundation
import NetworkExtension

autoreleasepool {
    NEProvider.startSystemExtensionMode()
}

dispatchMain()
