import {Profile, type ProfileGroup} from '../lib/profile'

import {importSpeedscopeProfiles} from '../lib/file-format'
import {type ProfileDataSource, TextProfileDataSource, MaybeCompressedDataReader} from './utils'
import {decodeBase64} from '../lib/utils'
import {isTraceEventFormatted, importTraceEvents} from './trace-event'
import {importFromChromeTimeline, isChromeTimeline, isChromeTimelineObject} from "@/speedscope/import/chrome";

export async function importProfileGroupFromText(
  fileName: string,
  contents: string,
): Promise<ProfileGroup | null> {
  return await importProfileGroup(new TextProfileDataSource(fileName, contents))
}

export async function importProfileGroupFromBase64(
  fileName: string,
  b64contents: string,
): Promise<ProfileGroup | null> {
  return await importProfileGroup(
    MaybeCompressedDataReader.fromArrayBuffer(fileName, decodeBase64(b64contents).buffer),
  )
}

export async function importProfilesFromFile(file: File): Promise<ProfileGroup | null> {
  return importProfileGroup(MaybeCompressedDataReader.fromFile(file))
}

export async function importProfilesFromArrayBuffer(
  fileName: string,
  buffer: ArrayBuffer,
): Promise<ProfileGroup | null> {
  return importProfileGroup(MaybeCompressedDataReader.fromArrayBuffer(fileName, buffer))
}

async function importProfileGroup(dataSource: ProfileDataSource): Promise<ProfileGroup | null> {
  const fileName = await dataSource.name()

  const profileGroup = await _importProfileGroup(dataSource)
  if (profileGroup) {
    if (!profileGroup.name) {
      profileGroup.name = fileName
    }
    for (let profile of profileGroup.profiles) {
      if (profile && !profile.getName()) {
        profile.setName(fileName)
      }
    }
    return profileGroup
  }
  return null
}

function toGroup(profile: Profile | null): ProfileGroup | null {
  if (!profile) return null
  return {name: profile.getName(), indexToView: 0, profiles: [profile]}
}

async function _importProfileGroup(dataSource: ProfileDataSource): Promise<ProfileGroup | null> {
  const fileName = await dataSource.name()

  const buffer = await dataSource.readAsArrayBuffer()

  const contents = await dataSource.readAsText()

  // First pass: Check known file format names to infer the file type
  if (fileName.endsWith('.speedscope.json')) {
    console.log('Importing as speedscope json file')
    return importSpeedscopeProfiles(contents.parseAsJSON())
  }

  // Second pass: Try to guess what file format it is based on structure
  let parsed: any
  try {
    parsed = contents.parseAsJSON()
  } catch (e) {}
  if (parsed) {
    if (parsed['$schema'] === 'https://www.speedscope.app/file-format-schema.json') {
      console.log('Importing as speedscope json file')
      return importSpeedscopeProfiles(parsed)
    } else if (isChromeTimeline(parsed)) {
      console.log('Importing as Chrome Timeline')
      return importFromChromeTimeline(parsed, fileName)
    } else if (isChromeTimelineObject(parsed)) {
      console.log('Importing as Chrome Timeline Object')
      return importFromChromeTimeline(parsed.traceEvents, fileName)
    } else if (isTraceEventFormatted(parsed)) {
      console.log('Importing as Trace Event Format profile')
      return importTraceEvents(parsed)
    }
  } else {
    // Format is not JSON
  }

  // Unrecognized format
  return null
}

