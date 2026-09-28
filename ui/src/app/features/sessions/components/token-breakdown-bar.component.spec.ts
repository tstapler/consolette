import { ComponentFixture, TestBed } from '@angular/core/testing';
import { TokenBreakdownBarComponent } from './token-breakdown-bar.component';

describe('TokenBreakdownBarComponent', () => {
  let component: TokenBreakdownBarComponent;
  let fixture: ComponentFixture<TokenBreakdownBarComponent>;

  beforeEach(async () => {
    await TestBed.configureTestingModule({
      imports: [TokenBreakdownBarComponent]
    }).compileComponents();

    fixture = TestBed.createComponent(TokenBreakdownBarComponent);
    component = fixture.componentInstance;
    component.promptTokens = 1000;
    component.responseTokens = 500;
    component.savedTokens = 500;
    fixture.detectChanges();
  });

  it('should create token breakdown bar component', () => {
    expect(component).toBeTruthy();
  });

  it('should calculate total and percentages correctly', () => {
    expect(component.totalTokens()).toBe(2000);
    expect(component.promptPercent()).toBe(50);
    expect(component.responsePercent()).toBe(25);
    expect(component.savedPercent()).toBe(25);
  });
});
