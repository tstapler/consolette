import { TestBed } from '@angular/core/testing';
import { ConfigStore } from './config.store';
import { HttpTestingController, provideHttpClientTesting } from '@angular/common/http/testing';
import { provideHttpClient } from '@angular/common/http';
import { ConfigData } from '../models/telemetry.models';

describe('ConfigStore (TC-UNIT-11)', () => {
  let store: ConfigStore;
  let httpMock: HttpTestingController;

  beforeEach(() => {
    TestBed.configureTestingModule({
      providers: [
        ConfigStore,
        provideHttpClient(),
        provideHttpClientTesting()
      ]
    });

    store = TestBed.inject(ConfigStore);
    httpMock = TestBed.inject(HttpTestingController);
  });

  afterEach(() => {
    httpMock.verify();
  });

  it('TC-UNIT-11: should load and apply config via REST API', () => {
    const mockConfig: ConfigData = {
      webUi: 'angular',
      fallbackCascade: ['anthropic', 'bedrock']
    };

    store.loadConfig();

    const getReq = httpMock.expectOne('/v1/dashboard/config');
    expect(getReq.request.method).toBe('GET');
    getReq.flush(mockConfig);

    expect(store.config()).toEqual(mockConfig);

    const updatedConfig: ConfigData = {
      ...mockConfig,
      fallbackCascade: ['anthropic', 'openai']
    };

    store.applyConfig(updatedConfig);
    expect(store.isSaving()).toBeTrue();

    const putReq = httpMock.expectOne('/v1/dashboard/config');
    expect(putReq.request.method).toBe('PUT');
    putReq.flush(updatedConfig);

    expect(store.isSaving()).toBeFalse();
    expect(store.config()).toEqual(updatedConfig);
    expect(store.lastApplied()).not.toBeNull();
  });
});
